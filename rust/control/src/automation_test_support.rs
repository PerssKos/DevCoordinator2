use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::database::Database;
use crate::platform::Clock;
use crate::repository::Registry;
use crate::review::ReviewService;
use crate::usage::UsageService;

pub(super) const START: u64 = 1_000_000;
pub(super) const WEEK: u64 = 7 * 86_400_000;

pub(super) struct Fixture {
    pub _temporary: tempfile::TempDir,
    pub database: Database,
    pub repository: PathBuf,
    pub config: Config,
    pub service: ReviewService,
}

pub(super) struct FixtureClock;
impl Clock for FixtureClock {
    fn now_utc(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(START + WEEK) * 1_000_000)
            .unwrap()
    }
}

impl Fixture {
    pub fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        let database = Database::open(root.join("authority.sqlite3")).unwrap();
        let path = repository.to_string_lossy().into_owned();
        database.transaction(move |transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('project-alpha',?1,'Project Alpha','1970-01-01T00:16:40Z',1,'1970-01-01T00:16:40Z')", [&path])?;
            transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','project-alpha',?1,'1970-01-01T00:16:40Z','1970-01-01T00:16:40Z')", [&path])?;
            transaction.execute_batch("INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES('spec','project-alpha',1,1,'Specify the workflow','Agree acceptance before implementation','improvement','in_progress','1970-01-01T00:16:40Z','fixture','1970-01-01T00:16:40Z');
                INSERT INTO decisions(decision_id,repository_id,seq,ref,aspect,title,body,created_at,created_by) VALUES('decision-quality','project-alpha',1,'QUALITY','testing','Keep release proof','Intentional release validation and user waiting are not waste.','1970-01-01T00:16:40Z','fixture');")?;
            Ok(())
        }).unwrap();
        let config = Config {
            socket_path: root.join("daemon.sock"),
            state_dir: root.join("state"),
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "fixture".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: vec![],
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: root.join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: Default::default(),
            codex_usage_sources_file: None,
            codex_usage_sources: vec![],
        };
        let usage = UsageService::with_clock(
            config.clone(),
            database.clone(),
            Registry::new(database.clone()),
            Arc::new(FixtureClock),
        );
        let service = ReviewService::new(database.clone(), usage);
        Self {
            _temporary: temporary,
            database,
            repository,
            config,
            service,
        }
    }
}
