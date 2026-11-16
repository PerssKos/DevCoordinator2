use std::collections::HashSet;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use devcoordinator2_api::configuration::{Change, Snapshot};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rustix::fs::{Mode, OFlags, open};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::{Config, read_compose_policy, valid_relative_path};
use crate::database::Database;
use crate::deployment_files::atomic_write;
use crate::platform::HostRandom;

type Authorizations = HashSet<(String, String)>;

#[derive(Clone)]
pub struct RuntimeConfiguration {
    path: Option<PathBuf>,
    active: Arc<Mutex<Authorizations>>,
    database: Database,
}

impl RuntimeConfiguration {
    pub fn new(config: &Config, database: Database) -> Self {
        Self {
            path: config.compose_env_allowlist_file.clone(),
            active: Arc::new(Mutex::new(config.compose_env_authorizations.clone())),
            database,
        }
    }

    pub fn authorized(&self, repository: &str, file: &str) -> bool {
        self.active
            .lock()
            .is_ok_and(|active| active.contains(&(repository.to_owned(), file.to_owned())))
    }

    pub fn get(&self) -> Result<Snapshot, ProtocolError> {
        let active = self.active.lock().map_err(|_| unavailable())?;
        self.snapshot(&active)
    }

    pub fn update(
        &self,
        repository: &str,
        file: &str,
        authorized: bool,
        expected_revision: &str,
        actor: &str,
    ) -> Result<Snapshot, ProtocolError> {
        if !valid_relative_path(file)
            || repository.len() != 17
            || !repository.starts_with('r')
            || !repository[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "invalid authorization target",
            ));
        }
        let mut active = self.active.lock().map_err(|_| unavailable())?;
        let previous = revision(&active);
        check_revision(expected_revision, &previous)?;
        let path = self.path.as_deref().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ConfigurationRestartRequired,
                "configure the private Compose policy location during installation before using live authorization updates",
            )
        })?;
        let stored = read_compose_policy(Some(path)).map_err(|_| invalid_policy())?;
        check_revision(&previous, &revision(&stored))?;
        let mut next = active.clone();
        let target = (repository.to_owned(), file.to_owned());
        let changed = if authorized {
            next.insert(target)
        } else {
            next.remove(&target)
        };
        if !changed {
            return self.snapshot(&active);
        }
        if next.len() > 256 {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "at most 256 Compose authorizations are supported",
            ));
        }
        let next_revision = revision(&next);
        let action = if authorized {
            "authorize_compose_env"
        } else {
            "revoke_compose_env"
        };
        self.record(action, "prepared", &previous, &next_revision, actor)?;
        let publication = (|| {
            let descriptor = open(
                path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| invalid_policy())?;
            let metadata = descriptor.metadata().map_err(|_| invalid_policy())?;
            let parent = path.parent().ok_or_else(invalid_policy)?;
            let directory = open(
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| invalid_policy())?;
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(invalid_policy)?;
            let payload = policy_bytes(&next);
            if payload.len() > 65_536 {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "Compose policy exceeds its size limit",
                ));
            }
            check_revision(
                &previous,
                &revision(&read_compose_policy(Some(path)).map_err(|_| invalid_policy())?),
            )?;
            atomic_write(
                &directory,
                filename,
                &payload,
                metadata.mode() & 0o777,
                metadata.uid(),
                metadata.gid(),
                &HostRandom,
            )
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::ConfigurationInvalid,
                    "cannot publish the private Compose policy; verify service write access to the configured policy directory",
                )
            })
        })();
        if let Err(error) = publication {
            self.record(action, "failed", &previous, &next_revision, actor)?;
            return Err(error);
        }
        *active = next;
        self.record(action, "activated", &previous, &next_revision, actor)?;
        self.snapshot(&active)
    }

    pub fn reload(&self, expected_revision: &str, actor: &str) -> Result<Snapshot, ProtocolError> {
        let mut active = self.active.lock().map_err(|_| unavailable())?;
        let previous = revision(&active);
        check_revision(expected_revision, &previous)?;
        let path = self.path.as_deref().ok_or_else(|| ProtocolError::new(
            ErrorCode::ConfigurationRestartRequired,
            "configure the private Compose policy location during installation before reloading it",
        ))?;
        let next = read_compose_policy(Some(path)).map_err(|_| invalid_policy())?;
        let next_revision = revision(&next);
        if previous != next_revision {
            self.record("reload", "prepared", &previous, &next_revision, actor)?;
            *active = next;
            self.record("reload", "activated", &previous, &next_revision, actor)?;
        }
        self.snapshot(&active)
    }

    fn snapshot(&self, active: &Authorizations) -> Result<Snapshot, ProtocolError> {
        let active_revision = revision(active);
        let disk = read_compose_policy(self.path.as_deref());
        let validation_error = disk.as_ref().err().map(|_| {
            "private Compose policy is unavailable or invalid; active settings are unchanged"
                .to_owned()
        });
        let stored_revision = disk.ok().map(|stored| revision(&stored));
        let history = self.database.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT sequence,action,outcome,previous_revision,revision,at FROM runtime_configuration_events ORDER BY sequence DESC LIMIT 20",
            )?;
            let changes = statement.query_map([], |row| Ok(Change {
                sequence: row.get::<_, i64>(0)? as u64, action: row.get(1)?, outcome: row.get(2)?,
                previous_revision: row.get(3)?, revision: row.get(4)?, at: row.get(5)?,
            }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(changes)
        }).map_err(|_| unavailable())?;
        Ok(Snapshot {
            configured: self.path.is_some(),
            pending_reload: stored_revision
                .as_ref()
                .is_some_and(|stored| stored != &active_revision),
            active_revision,
            stored_revision,
            authorization_count: active.len(),
            reloadable_settings: vec!["compose_env_authorizations".to_owned()],
            restart_required_settings: vec![
                "compose_env_allowlist_file".to_owned(),
                "other_instance_settings".to_owned(),
            ],
            validation_error,
            history,
        })
    }

    fn record(
        &self,
        action: &str,
        outcome: &str,
        previous: &str,
        revision: &str,
        actor: &str,
    ) -> Result<(), ProtocolError> {
        let values = [action, outcome, previous, revision, actor].map(str::to_owned);
        self.database.transaction(move |connection| {
            connection.execute(
                "INSERT INTO runtime_configuration_events(action,outcome,previous_revision,revision,actor,at) VALUES($1,$2,$3,$4,$5,to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
                crate::database::params![values[0],values[1],values[2],values[3],values[4]],
            )?;
            Ok(())
        }).map_err(|_| unavailable())
    }
}

fn policy_bytes(authorizations: &Authorizations) -> Vec<u8> {
    let mut entries = authorizations.iter().collect::<Vec<_>>();
    entries.sort();
    serde_json::to_vec(&json!({
        "schema":1,
        "authorizations":entries.into_iter().map(|(repository,file)| json!({"repository_id":repository,"path":file})).collect::<Vec<_>>()
    })).expect("fixed policy serialization")
}

fn revision(authorizations: &Authorizations) -> String {
    Sha256::digest(policy_bytes(authorizations))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn check_revision(expected: &str, actual: &str) -> Result<(), ProtocolError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::ConfigurationConflict,
            "configuration changed; inspect current settings before retrying",
        ))
    }
}

fn invalid_policy() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::ConfigurationInvalid,
        "private Compose policy is unavailable or invalid; active settings are unchanged",
    )
}

fn unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "configuration state or its permanent history is unavailable; inspect before retrying",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const REPOSITORY: &str = "r1111111111111111";

    fn fixture() -> (tempfile::TempDir, RuntimeConfiguration) {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("policy.json");
        std::fs::write(&path, policy_bytes(&HashSet::new())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let configuration = RuntimeConfiguration {
            path: Some(path),
            active: Arc::new(Mutex::new(HashSet::new())),
            database,
        };
        (temporary, configuration)
    }

    #[test]
    fn exact_updates_activate_persist_preserve_and_revoke_without_restart() {
        let (_temporary, configuration) = fixture();
        let initial = configuration.get().unwrap();
        let first = configuration
            .update(
                REPOSITORY,
                ".env",
                true,
                &initial.active_revision,
                "uid:1000",
            )
            .unwrap();
        assert!(configuration.authorized(REPOSITORY, ".env"));
        assert!(!first.pending_reload);
        assert_eq!(first.stored_revision.as_ref(), Some(&first.active_revision));
        let second = configuration
            .update(
                REPOSITORY,
                "deploy/other.env",
                true,
                &first.active_revision,
                "uid:1000",
            )
            .unwrap();
        assert!(configuration.authorized(REPOSITORY, ".env"));
        assert_eq!(second.authorization_count, 2);
        let disk = read_compose_policy(configuration.path.as_deref()).unwrap();
        assert_eq!(disk.len(), 2);
        assert_eq!(
            std::fs::metadata(configuration.path.as_ref().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o640
        );
        let unchanged = configuration
            .update(
                REPOSITORY,
                ".env",
                true,
                &second.active_revision,
                "uid:1000",
            )
            .unwrap();
        assert_eq!(unchanged.history.len(), second.history.len());
        let revoked = configuration
            .update(
                REPOSITORY,
                ".env",
                false,
                &second.active_revision,
                "uid:1000",
            )
            .unwrap();
        assert!(!configuration.authorized(REPOSITORY, ".env"));
        assert!(configuration.authorized(REPOSITORY, "deploy/other.env"));
        assert_eq!(revoked.authorization_count, 1);
        assert_eq!(revoked.history[0].outcome, "activated");
    }

    #[test]
    fn conflicting_updates_and_invalid_reload_preserve_active_configuration() {
        let (_temporary, configuration) = fixture();
        let initial = configuration.get().unwrap();
        let active = configuration
            .update(
                REPOSITORY,
                ".env",
                true,
                &initial.active_revision,
                "uid:1000",
            )
            .unwrap();
        assert_eq!(
            configuration
                .update(
                    REPOSITORY,
                    "other.env",
                    true,
                    &initial.active_revision,
                    "uid:1000"
                )
                .unwrap_err()
                .code,
            ErrorCode::ConfigurationConflict
        );
        std::fs::write(
            configuration.path.as_ref().unwrap(),
            b"{\"credential\":\"private-marker-never-disclose\"}",
        )
        .unwrap();
        let error = configuration
            .reload(&active.active_revision, "uid:1000")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigurationInvalid);
        assert!(!format!("{error:?}").contains("private-marker"));
        assert!(configuration.authorized(REPOSITORY, ".env"));
        let status = configuration.get().unwrap();
        assert_eq!(status.active_revision, active.active_revision);
        assert!(status.validation_error.is_some());
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("private-marker")
        );
    }

    #[test]
    fn external_changes_require_explicit_reload_and_concurrent_updates_conflict() {
        let (_temporary, configuration) = fixture();
        let initial = configuration.get().unwrap();
        let policy = HashSet::from([(REPOSITORY.to_owned(), "external.env".to_owned())]);
        std::fs::write(configuration.path.as_ref().unwrap(), policy_bytes(&policy)).unwrap();
        assert!(configuration.get().unwrap().pending_reload);
        assert_eq!(
            configuration
                .update(
                    REPOSITORY,
                    ".env",
                    true,
                    &initial.active_revision,
                    "uid:1000"
                )
                .unwrap_err()
                .code,
            ErrorCode::ConfigurationConflict
        );
        let reloaded = configuration
            .reload(&initial.active_revision, "uid:1000")
            .unwrap();
        assert!(configuration.authorized(REPOSITORY, "external.env"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let jobs = ["first.env", "second.env"].map(|file| {
            let configuration = configuration.clone();
            let barrier = barrier.clone();
            let expected = reloaded.active_revision.clone();
            std::thread::spawn(move || {
                barrier.wait();
                configuration.update(REPOSITORY, file, true, &expected, "uid:1000")
            })
        });
        let results = jobs.map(|job| job.join().unwrap());
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| error.code == ErrorCode::ConfigurationConflict)
        );
        assert_eq!(configuration.get().unwrap().authorization_count, 2);
    }

    #[test]
    fn unsafe_policy_replacement_and_unsafe_targets_are_not_activated() {
        let (temporary, configuration) = fixture();
        let initial = configuration.get().unwrap();
        for file in ["../other.env", "/absolute.env", "nested//file.env"] {
            assert_eq!(
                configuration
                    .update(REPOSITORY, file, true, &initial.active_revision, "uid:1000")
                    .unwrap_err()
                    .code,
                ErrorCode::ParamsInvalid
            );
        }
        let policy = configuration.path.as_ref().unwrap();
        let other = temporary.path().join("other.json");
        std::fs::write(&other, policy_bytes(&HashSet::new())).unwrap();
        std::fs::remove_file(policy).unwrap();
        std::os::unix::fs::symlink(&other, policy).unwrap();
        assert_eq!(
            configuration
                .reload(&initial.active_revision, "uid:1000")
                .unwrap_err()
                .code,
            ErrorCode::ConfigurationInvalid
        );
        assert_eq!(
            configuration.get().unwrap().active_revision,
            initial.active_revision
        );
        assert_eq!(
            std::fs::read(&other).unwrap(),
            policy_bytes(&HashSet::new())
        );
    }

    #[test]
    fn unconfigured_policy_location_is_explicitly_restart_required() {
        let (_temporary, mut configuration) = fixture();
        configuration.path = None;
        let status = configuration.get().unwrap();
        assert!(!status.configured);
        assert_eq!(
            configuration
                .reload(&status.active_revision, "uid:1000")
                .unwrap_err()
                .code,
            ErrorCode::ConfigurationRestartRequired
        );
        assert_eq!(
            configuration
                .update(
                    REPOSITORY,
                    ".env",
                    true,
                    &status.active_revision,
                    "uid:1000"
                )
                .unwrap_err()
                .code,
            ErrorCode::ConfigurationRestartRequired
        );
        assert_eq!(
            configuration.get().unwrap().active_revision,
            status.active_revision
        );
    }
}
