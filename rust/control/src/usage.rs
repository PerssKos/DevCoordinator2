//! Privacy-preserving read-only aggregation of configured Codex usage collectors.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::params::{
    UsageRange, UsageRepositories as UsageRepositoriesParams,
    UsageRepository as UsageRepositoryParams,
};
use devcoordinator2_api::results::{
    CoverageState, MeasuredTime, PhaseTime, ToolFamily, ToolOutcome, UsageActivity, UsageCoverage,
    UsageRepositories, UsageRepository, UsageRepositoryRow, UsageSemantics, UsageSeriesPoint,
    UsageTime, UsageTools, UsageTotals,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value as SqlValue,
};
use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use time::{format_description::FormatItem, macros::format_description};

use crate::config::{CodexUsageSource, Config};
use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;

#[path = "usage_cache.rs"]
mod cache;

const SUPPORTED_DATABASE_SCHEMAS: &[u32] = &[4, 5, 6];
const SUPPORTED_TAXONOMY: u32 = 1;
const SOURCE_OUTPUT_BYTES: usize = 256 * 1024;
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);
const PROCESS_POLL: Duration = Duration::from_millis(10);
const SQLITE_VARIABLE_CHUNK: usize = 20_000;
const PHASES: &[&str] = &[
    "planning",
    "implementation",
    "testing",
    "deployment",
    "reporting",
    "unattributed",
];
const TOKEN_CATEGORIES: &[(&str, TokenField)] = &[
    ("total_tokens", TokenField::Total),
    ("input_tokens", TokenField::Input),
    (
        "input_tokens_details.cached_tokens",
        TokenField::CachedInput,
    ),
    ("output_tokens", TokenField::Output),
    (
        "output_tokens_details.reasoning_tokens",
        TokenField::Reasoning,
    ),
];
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone)]
pub struct UsageService {
    registry: Registry,
    usage: CodexUsage,
}

#[derive(Clone)]
pub struct CodexUsage {
    config: Arc<Config>,
    authority: Database,
    clock: Arc<dyn Clock>,
    probe: Arc<dyn RepositoryProbe>,
    cache: cache::UsageCache,
}

#[derive(Clone, Debug)]
pub struct RepositoryRecord {
    pub repository_id: String,
    pub display_name: String,
    pub root_path: PathBuf,
}

pub trait RepositoryProbe: Send + Sync + 'static {
    fn probe(
        &self,
        source: &CodexUsageSource,
        repository: &Path,
        now_ms: u64,
    ) -> Result<(String, u32, u32), String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostRepositoryProbe;

#[derive(Clone, Copy, Debug)]
enum TokenField {
    Total,
    Input,
    CachedInput,
    Output,
    Reasoning,
}

#[derive(Default)]
struct SourceReport {
    database_schema: u32,
    taxonomy_version: u32,
    evidence: bool,
    freshest_at_ms: Option<u64>,
    tokens: BTreeMap<String, u64>,
    token_observations: BTreeMap<String, u64>,
    phase_series: Vec<BTreeMap<String, u64>>,
    token_buckets_observed: Vec<bool>,
    bucket_coverage: Vec<CoverageState>,
    activities: BTreeMap<(String, String), u64>,
    activity_operations: BTreeMap<(String, String), u64>,
    activity_provenance: BTreeMap<(String, String, String), u64>,
    operation_count: u64,
    model_request_count: u64,
    tool_count: u64,
    tool_outcomes: BTreeMap<String, u64>,
    tool_families: BTreeMap<String, u64>,
    coverage_events: BTreeMap<String, u64>,
    request_intervals: Vec<(u64, u64)>,
    execution_intervals: Vec<(u64, u64)>,
    agent_intervals: BTreeMap<String, Vec<(u64, u64)>>,
    phase_intervals: BTreeMap<String, Vec<(u64, u64)>>,
    request_unknown: u64,
    execution_unknown: u64,
    agent_unknown: u64,
    phase_unknown: BTreeMap<String, u64>,
}

#[derive(Clone)]
struct Operation {
    id: String,
    kind: String,
    agent_id: Option<String>,
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    phase: String,
    activity: String,
    activity_state: String,
    provenance: String,
    terminal_event: Option<String>,
    tool_family: Option<String>,
}

impl UsageService {
    pub fn new(config: Config, database: Database, registry: Registry) -> Self {
        Self::with_clock(config, database, registry, Arc::new(HostClock))
    }

    pub fn with_clock(
        config: Config,
        database: Database,
        registry: Registry,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            registry,
            usage: CodexUsage::with_probe(config, database, clock, Arc::new(HostRepositoryProbe)),
        }
    }

    pub fn usage(&self) -> &CodexUsage {
        &self.usage
    }

    pub fn repositories(
        &self,
        params: UsageRepositoriesParams,
    ) -> Result<UsageRepositories, ProtocolError> {
        let records = self.records()?;
        let report = self.usage.repositories(&records, params.range.clone())?;
        if params.wait_for_refresh {
            self.usage.wait_for_refresh(None);
            return self.usage.repositories(&records, params.range);
        }
        Ok(report)
    }

    pub fn repository(
        &self,
        params: UsageRepositoryParams,
    ) -> Result<UsageRepository, ProtocolError> {
        let repository = self
            .records()?
            .into_iter()
            .find(|record| record.repository_id == params.repository_id)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::RepositoryNotFound, "no registered repository")
            })?;
        let report = self.usage.repository(&repository, params.range.clone())?;
        if params.wait_for_refresh {
            self.usage.wait_for_refresh(Some(&repository.repository_id));
            return self.usage.repository(&repository, params.range);
        }
        Ok(report)
    }

    pub(crate) fn review_window(
        &self,
        repository: &RepositoryRecord,
        start_ms: u64,
        end_ms: u64,
        deadline: Instant,
    ) -> Result<UsageRepository, ProtocolError> {
        self.usage.repository_window(
            repository,
            UsageRange::Hours24,
            self.usage.now_ms()?,
            start_ms,
            end_ms,
            end_ms - start_ms,
            1,
            false,
            Some(deadline),
        )
    }

    fn records(&self) -> Result<Vec<RepositoryRecord>, ProtocolError> {
        Ok(self
            .registry
            .list_repositories(false)?
            .repositories
            .into_iter()
            .map(|repository| RepositoryRecord {
                repository_id: repository.repository_id,
                display_name: repository.display_name,
                root_path: repository.root_path.into(),
            })
            .collect())
    }
}

impl CodexUsage {
    pub fn new(config: Config, authority: Database) -> Self {
        Self::with_probe(
            config,
            authority,
            Arc::new(HostClock),
            Arc::new(HostRepositoryProbe),
        )
    }

    pub fn with_probe(
        config: Config,
        authority: Database,
        clock: Arc<dyn Clock>,
        probe: Arc<dyn RepositoryProbe>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            authority,
            clock,
            probe,
            cache: cache::UsageCache::default(),
        }
    }

    pub fn repositories(
        &self,
        repositories: &[RepositoryRecord],
        range: UsageRange,
    ) -> Result<UsageRepositories, ProtocolError> {
        let now_ms = self.now_ms()?;
        let mut rows = Vec::new();
        for repository in repositories {
            let report = self.repository(repository, range.clone())?;
            rows.push(UsageRepositoryRow {
                repository_id: report.repository_id,
                display_name: report.display_name,
                range: report.range,
                coverage: report.coverage,
                total_tokens: report.totals.total_tokens,
                model_requests: report.totals.model_requests,
                tool_calls: report.totals.tool_calls,
                execution_wall_ms: report.time.execution_wall.measured_ms,
            });
        }
        Ok(UsageRepositories {
            range,
            generated_at_ms: now_ms,
            repositories: rows,
        })
    }

    pub fn wait_for_refresh(&self, repository_id: Option<&str>) {
        self.cache.wait(repository_id);
    }

    pub fn repository(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
    ) -> Result<UsageRepository, ProtocolError> {
        let now_ms = self.now_ms()?;
        let (start, end, bucket, count) = usage_window(&range, now_ms);
        self.cached_window(repository, range, now_ms, start, end, bucket, count, true)
    }

    pub fn repository_at(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        now_ms: u64,
    ) -> Result<UsageRepository, ProtocolError> {
        let (start, end, bucket, count) = usage_window(&range, now_ms);
        self.repository_window(
            repository, range, now_ms, start, end, bucket, count, true, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn repository_buckets(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        bucket_ms: u64,
        bucket_count: usize,
        now_ms: u64,
        aligned_end_ms: u64,
        resolve_missing: bool,
    ) -> Result<UsageRepository, ProtocolError> {
        if bucket_ms < 60_000
            || !(1..=400).contains(&bucket_count)
            || aligned_end_ms < now_ms
            || aligned_end_ms.saturating_sub(now_ms) >= bucket_ms
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "invalid Codex usage bucket window",
            ));
        }
        let start = aligned_end_ms.saturating_sub(
            bucket_ms.saturating_mul(u64::try_from(bucket_count).unwrap_or(u64::MAX)),
        );
        self.cached_window(
            repository,
            range,
            now_ms,
            start,
            now_ms,
            bucket_ms,
            bucket_count,
            resolve_missing,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn cached_window(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        now_ms: u64,
        start_ms: u64,
        end_ms: u64,
        bucket_ms: u64,
        bucket_count: usize,
        resolve_missing: bool,
    ) -> Result<UsageRepository, ProtocolError> {
        let key = format!(
            "{}:{:?}:{}:{start_ms}:{bucket_ms}:{bucket_count}",
            repository.repository_id,
            repository.root_path,
            range_name(&range)
        );
        let empty = combine(
            repository,
            range.clone(),
            now_ms,
            start_ms,
            bucket_ms,
            bucket_count,
            &[],
            BTreeMap::new(),
            self.config.codex_usage_sources.len(),
        );
        let usage = self.clone();
        let repository = repository.clone();
        Ok(self.cache.get(key, empty, move || {
            usage.repository_window(
                &repository,
                range,
                now_ms,
                start_ms,
                end_ms,
                bucket_ms,
                bucket_count,
                resolve_missing,
                None,
            )
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn repository_window(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        now_ms: u64,
        start_ms: u64,
        end_ms: u64,
        bucket_ms: u64,
        bucket_count: usize,
        resolve_missing: bool,
        deadline: Option<Instant>,
    ) -> Result<UsageRepository, ProtocolError> {
        let mut reports = Vec::new();
        let mut failures = BTreeMap::new();
        for source in &self.config.codex_usage_sources {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                increment(&mut failures, "query_budget_exhausted");
                continue;
            }
            match self.repository_key(source, repository, now_ms, resolve_missing) {
                Ok(key) => {
                    match self.read_source(
                        source,
                        &key,
                        start_ms,
                        end_ms,
                        bucket_ms,
                        bucket_count,
                        deadline,
                    ) {
                        Ok(report) => reports.push((source.uid, report)),
                        Err(reason) if reason == "mapping_unavailable" && resolve_missing => {
                            self.delete_link(source.uid, &repository.repository_id)?;
                            match self
                                .repository_key(source, repository, now_ms, true)
                                .and_then(|key| {
                                    self.read_source(
                                        source,
                                        &key,
                                        start_ms,
                                        end_ms,
                                        bucket_ms,
                                        bucket_count,
                                        deadline,
                                    )
                                    .map_err(source_error)
                                }) {
                                Ok(report) => reports.push((source.uid, report)),
                                Err(error) => increment(&mut failures, &error.message),
                            }
                        }
                        Err(reason) => increment(&mut failures, &reason),
                    }
                }
                Err(error) => increment(&mut failures, &error.message),
            }
        }
        Ok(combine(
            repository,
            range,
            now_ms,
            start_ms,
            bucket_ms,
            bucket_count,
            &reports,
            failures,
            self.config.codex_usage_sources.len(),
        ))
    }

    fn repository_key(
        &self,
        source: &CodexUsageSource,
        repository: &RepositoryRecord,
        now_ms: u64,
        resolve_missing: bool,
    ) -> Result<String, ProtocolError> {
        let uid = source.uid;
        let repository_id = repository.repository_id.clone();
        let cached = self
            .authority
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT codex_repository_id FROM codex_usage_repository_links WHERE source_uid=?1 AND repository_id=?2",
                        rusqlite::params![uid,repository_id],
                        |row| row.get::<_,String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        if !resolve_missing {
            return cached.ok_or_else(|| source_error("mapping_pending"));
        }
        let (key, schema, taxonomy) = self
            .probe
            .probe(source, &repository.root_path, now_ms)
            .map_err(source_error)?;
        if !valid_repository_key(&key) {
            return Err(source_error("source_unavailable"));
        }
        let resolved_at = self.timestamp()?;
        let source_uid = source.uid;
        let repository_id = repository.repository_id.clone();
        let stored_key = key.clone();
        self.authority
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT OR REPLACE INTO codex_usage_repository_links(source_uid,repository_id,codex_repository_id,source_schema,taxonomy_version,resolved_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![source_uid,repository_id,stored_key,schema,taxonomy,resolved_at],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        Ok(key)
    }

    fn delete_link(&self, uid: u32, repository_id: &str) -> Result<(), ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.authority
            .transaction(move |transaction| {
                transaction.execute(
                    "DELETE FROM codex_usage_repository_links WHERE source_uid=?1 AND repository_id=?2",
                    rusqlite::params![uid,repository_id],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn read_source(
        &self,
        source: &CodexUsageSource,
        repository_key: &str,
        start_ms: u64,
        end_ms: u64,
        bucket_ms: u64,
        bucket_count: usize,
        deadline: Option<Instant>,
    ) -> Result<SourceReport, String> {
        let inherited_deadline = deadline;
        let deadline = deadline.unwrap_or_else(|| Instant::now() + QUERY_TIMEOUT);
        if Instant::now() >= deadline {
            return Err("query_budget_exhausted".into());
        }
        let result = (|| {
            let connection = match inherited_deadline {
                Some(deadline) => open_source_until(source, deadline)?,
                None => open_source(source)?,
            };
            let schema = maximum_version(&connection, "_sqlx_migrations")?;
            let taxonomy = maximum_version(&connection, "taxonomy_versions")?;
            if !SUPPORTED_DATABASE_SCHEMAS.contains(&schema) || taxonomy != SUPPORTED_TAXONOMY {
                return Err("schema_unsupported".into());
            }
            let canonical = canonical_repository(&connection, repository_key)?;
            let family = repository_family(&connection, &canonical)?;
            if !repository_exists(&connection, &family)? {
                return Err("mapping_unavailable".into());
            }
            source_report(
                &connection,
                &family,
                schema,
                taxonomy,
                start_ms,
                end_ms,
                bucket_ms,
                bucket_count,
            )
        })();
        if Instant::now() >= deadline {
            Err("query_budget_exhausted".into())
        } else {
            result
        }
    }

    fn now_ms(&self) -> Result<u64, ProtocolError> {
        u64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000_000).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "usage timestamp is before Unix epoch",
            )
        })
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format usage timestamp")
                    .with_detail(error.to_string())
            })
    }
}

fn open_source(source: &CodexUsageSource) -> Result<Connection, String> {
    open_source_until(source, Instant::now() + QUERY_TIMEOUT)
}

fn open_source_until(source: &CodexUsageSource, deadline: Instant) -> Result<Connection, String> {
    let path = source.codex_home.join("usage/usage.sqlite3");
    let descriptor = open(
        &path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| "source_unavailable".to_owned())?;
    let metadata = fstat(&descriptor).map_err(|_| "source_unavailable".to_owned())?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != source.uid
    {
        return Err("source_unavailable".into());
    }
    #[cfg(target_os = "linux")]
    let exact_path = PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let exact_path = path;
    let connection = Connection::open_with_flags(
        exact_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "source_unavailable".to_owned())?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|_| "source_unavailable".to_owned())?;
    connection
        .busy_timeout(Duration::from_millis(250))
        .map_err(|_| "source_unavailable".to_owned())?;
    connection
        .progress_handler(10_000, Some(move || Instant::now() > deadline))
        .map_err(|_| "source_unavailable".to_owned())?;
    Ok(connection)
}

fn maximum_version(connection: &Connection, table: &str) -> Result<u32, String> {
    if !matches!(table, "_sqlx_migrations" | "taxonomy_versions") {
        return Err("source_unavailable".into());
    }
    let value = connection
        .query_row(
            &format!("SELECT COALESCE(MAX(version),0) FROM {table}"),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| "source_unavailable".to_owned())?;
    u32::try_from(value).map_err(|_| "source_unavailable".into())
}

fn canonical_repository(connection: &Connection, key: &str) -> Result<String, String> {
    if !valid_repository_key(key) {
        return Err("mapping_unavailable".into());
    }
    let mut current = key.to_owned();
    let mut seen = BTreeSet::new();
    while seen.len() < 64 && seen.insert(current.clone()) {
        let target = connection
            .query_row(
                "SELECT target_repository_id FROM repository_merge_events WHERE source_repository_id=?1",
                [&current],
                |row| row.get::<_,String>(0),
            )
            .optional()
            .map_err(|_| "source_unavailable".to_owned())?;
        let Some(target) = target else {
            return Ok(current);
        };
        if !valid_repository_key(&target) {
            return Err("source_unavailable".into());
        }
        current = target;
    }
    Err("source_unavailable".into())
}

fn repository_family(connection: &Connection, canonical: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(
            "WITH RECURSIVE family(id) AS (SELECT ?1 UNION ALL SELECT merge.source_repository_id FROM repository_merge_events merge JOIN family ON merge.target_repository_id=family.id) SELECT id FROM family",
        )
        .map_err(|_| "source_unavailable".to_owned())?;
    let rows = statement
        .query_map([canonical], |row| row.get::<_, String>(0))
        .map_err(|_| "source_unavailable".to_owned())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "source_unavailable".to_owned())?;
    if rows.is_empty() || rows.len() > 1_024 || rows.iter().any(|key| !valid_repository_key(key)) {
        return Err("source_unavailable".into());
    }
    Ok(rows)
}

fn repository_exists(connection: &Connection, family: &[String]) -> Result<bool, String> {
    let sql = format!(
        "SELECT 1 FROM repositories WHERE id IN ({}) LIMIT 1",
        placeholders(family.len())
    );
    connection
        .query_row(&sql, params_from_iter(family.iter().cloned()), |_| Ok(()))
        .optional()
        .map(|row| row.is_some())
        .map_err(|_| "source_unavailable".into())
}

#[allow(clippy::too_many_arguments)]
fn source_report(
    connection: &Connection,
    family: &[String],
    schema: u32,
    taxonomy: u32,
    start_ms: u64,
    end_ms: u64,
    bucket_ms: u64,
    bucket_count: usize,
) -> Result<SourceReport, String> {
    let mut values = family
        .iter()
        .cloned()
        .map(SqlValue::Text)
        .collect::<Vec<_>>();
    values.push(SqlValue::Integer(i64_value(end_ms)?));
    values.push(SqlValue::Integer(i64_value(start_ms)?));
    let sql = format!(
        "SELECT operation.id,operation.operation_kind,operation.agent_id,operation.started_at_ms,operation.phase,operation.activity,operation.activity_state,operation.attribution_provenance,terminal.occurred_at_ms,terminal.event_kind,tool.operation_family,tool.id,request.id FROM operations operation LEFT JOIN operation_events terminal ON terminal.operation_id=operation.id AND terminal.terminal=1 LEFT JOIN tool_invocations tool ON tool.operation_id=operation.id LEFT JOIN model_requests request ON request.operation_id=operation.id WHERE operation.id IN (SELECT attribution.operation_id FROM repository_attributions attribution WHERE attribution.repository_id IN ({})) AND operation.started_at_ms<? AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms>?)",
        placeholders(family.len())
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| "source_unavailable".to_owned())?;
    let mut rows = statement
        .query(params_from_iter(values))
        .map_err(|_| "source_unavailable".to_owned())?;
    let mut operations_by_id = BTreeMap::new();
    let mut request_operations = HashMap::new();
    let mut tool_operations = HashMap::new();
    while let Some(row) = rows.next().map_err(|_| "source_unavailable".to_owned())? {
        let id = row
            .get::<_, String>(0)
            .map_err(|_| "source_unavailable".to_owned())?;
        if let Some(tool_id) = row
            .get::<_, Option<String>>(11)
            .map_err(|_| "source_unavailable".to_owned())?
        {
            tool_operations.insert(tool_id, id.clone());
        }
        if let Some(request_id) = row
            .get::<_, Option<String>>(12)
            .map_err(|_| "source_unavailable".to_owned())?
        {
            request_operations.insert(request_id, id.clone());
        }
        if operations_by_id.contains_key(&id) {
            continue;
        }
        operations_by_id.insert(
            id.clone(),
            Operation {
                id,
                kind: row.get(1).map_err(|_| "source_unavailable".to_owned())?,
                agent_id: row.get(2).map_err(|_| "source_unavailable".to_owned())?,
                started_at_ms: u64::try_from(
                    row.get::<_, i64>(3)
                        .map_err(|_| "source_unavailable".to_owned())?,
                )
                .map_err(|_| "source_unavailable".to_owned())?,
                finished_at_ms: row
                    .get::<_, Option<i64>>(8)
                    .map_err(|_| "source_unavailable".to_owned())?
                    .and_then(|value| u64::try_from(value).ok()),
                phase: safe_phase(
                    &row.get::<_, String>(4)
                        .map_err(|_| "source_unavailable".to_owned())?,
                ),
                activity: safe_label(
                    &row.get::<_, String>(5)
                        .map_err(|_| "source_unavailable".to_owned())?,
                ),
                activity_state: safe_label(
                    &row.get::<_, String>(6)
                        .map_err(|_| "source_unavailable".to_owned())?,
                ),
                provenance: safe_label(
                    &row.get::<_, String>(7)
                        .map_err(|_| "source_unavailable".to_owned())?,
                ),
                terminal_event: row.get(9).map_err(|_| "source_unavailable".to_owned())?,
                tool_family: row
                    .get::<_, Option<String>>(10)
                    .map_err(|_| "source_unavailable".to_owned())?
                    .map(|value| safe_label(&value)),
            },
        );
    }
    drop(rows);
    drop(statement);
    let operation_ids = operations_by_id.keys().cloned().collect::<Vec<_>>();
    for identifiers in operation_ids.chunks(SQLITE_VARIABLE_CHUNK) {
        let sql = format!(
            "SELECT operation_id,phase,activity,activity_state,provenance FROM effective_classification_events WHERE operation_id IN ({})",
            placeholders(identifiers.len())
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| "source_unavailable".to_owned())?;
        let mut rows = statement
            .query(params_from_iter(identifiers.iter().cloned()))
            .map_err(|_| "source_unavailable".to_owned())?;
        while let Some(row) = rows.next().map_err(|_| "source_unavailable".to_owned())? {
            let operation_id = row
                .get::<_, String>(0)
                .map_err(|_| "source_unavailable".to_owned())?;
            let Some(operation) = operations_by_id.get_mut(&operation_id) else {
                continue;
            };
            operation.phase = safe_phase(
                &row.get::<_, String>(1)
                    .map_err(|_| "source_unavailable".to_owned())?,
            );
            operation.activity = safe_label(
                &row.get::<_, String>(2)
                    .map_err(|_| "source_unavailable".to_owned())?,
            );
            operation.activity_state = safe_label(
                &row.get::<_, String>(3)
                    .map_err(|_| "source_unavailable".to_owned())?,
            );
            operation.provenance = safe_label(
                &row.get::<_, String>(4)
                    .map_err(|_| "source_unavailable".to_owned())?,
            );
        }
    }
    let mut report = SourceReport {
        database_schema: schema,
        taxonomy_version: taxonomy,
        phase_series: vec![BTreeMap::new(); bucket_count],
        token_buckets_observed: vec![false; bucket_count],
        bucket_coverage: vec![CoverageState::Unobserved; bucket_count],
        ..Default::default()
    };
    let mut by_id = HashMap::new();
    for operation in operations_by_id.into_values() {
        report.evidence = true;
        report.operation_count = report.operation_count.saturating_add(1);
        report.freshest_at_ms = Some(
            report
                .freshest_at_ms
                .unwrap_or(0)
                .max(operation.started_at_ms),
        );
        if operation.kind == "model_request" {
            report.model_request_count = report.model_request_count.saturating_add(1);
        }
        if matches!(
            operation.kind.as_str(),
            "local_tool" | "hosted_tool" | "activity_control"
        ) {
            report.tool_count = report.tool_count.saturating_add(1);
            increment(
                &mut report.tool_outcomes,
                tool_outcome(operation.terminal_event.as_deref()),
            );
            increment(
                &mut report.tool_families,
                operation.tool_family.as_deref().unwrap_or("unknown"),
            );
        }
        increment_pair(
            &mut report.activity_operations,
            (&operation.phase, &operation.activity),
            1,
        );
        increment_triple(
            &mut report.activity_provenance,
            (&operation.phase, &operation.activity, &operation.provenance),
            1,
        );
        by_id.insert(operation.id.clone(), operation);
    }
    add_tokens(
        connection,
        &mut report,
        &by_id,
        &request_operations,
        &tool_operations,
        family,
        start_ms,
        end_ms,
        bucket_ms,
        bucket_count,
    )?;
    add_coverage(
        connection,
        &mut report,
        &by_id,
        start_ms,
        end_ms,
        bucket_ms,
        bucket_count,
    )?;
    add_intervals(connection, &mut report, &by_id, start_ms, end_ms)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn add_tokens(
    connection: &Connection,
    report: &mut SourceReport,
    operations: &HashMap<String, Operation>,
    request_operations: &HashMap<String, String>,
    tool_operations: &HashMap<String, String>,
    family: &[String],
    start_ms: u64,
    end_ms: u64,
    bucket_ms: u64,
    bucket_count: usize,
) -> Result<(), String> {
    for (column, source_operations) in [
        ("model_request_id", request_operations),
        ("tool_invocation_id", tool_operations),
    ] {
        let mut identifiers = source_operations.keys().cloned().collect::<Vec<_>>();
        identifiers.sort();
        for identifier_chunk in identifiers.chunks(SQLITE_VARIABLE_CHUNK) {
            let mut values = identifier_chunk
                .iter()
                .cloned()
                .map(SqlValue::Text)
                .collect::<Vec<_>>();
            values.extend(family.iter().cloned().map(SqlValue::Text));
            values.extend(
                TOKEN_CATEGORIES
                    .iter()
                    .map(|(name, _)| SqlValue::Text((*name).into())),
            );
            values.push(SqlValue::Integer(i64_value(start_ms)?));
            values.push(SqlValue::Integer(i64_value(end_ms)?));
            let sql = format!(
                "SELECT category_path,token_count,coverage_state,observed_at_ms,{column} FROM token_observations WHERE {column} IN ({}) AND repository_bucket IN ({}) AND category_path IN ({}) AND measurement_provenance='provider_reported' AND observed_at_ms>=? AND observed_at_ms<?",
                placeholders(identifier_chunk.len()),
                placeholders(family.len()),
                placeholders(TOKEN_CATEGORIES.len()),
            );
            let mut statement = connection
                .prepare(&sql)
                .map_err(|_| "source_unavailable".to_owned())?;
            let mut rows = statement
                .query(params_from_iter(values))
                .map_err(|_| "source_unavailable".to_owned())?;
            while let Some(row) = rows.next().map_err(|_| "source_unavailable".to_owned())? {
                let source_id = row
                    .get::<_, String>(4)
                    .map_err(|_| "source_unavailable".to_owned())?;
                let Some(operation) = source_operations
                    .get(&source_id)
                    .and_then(|operation_id| operations.get(operation_id))
                else {
                    continue;
                };
                let category = row
                    .get::<_, String>(0)
                    .map_err(|_| "source_unavailable".to_owned())?;
                if !TOKEN_CATEGORIES
                    .iter()
                    .any(|(allowed, _)| *allowed == category)
                {
                    continue;
                }
                let coverage = safe_coverage(
                    &row.get::<_, String>(2)
                        .map_err(|_| "source_unavailable".to_owned())?,
                );
                let observed = u64::try_from(
                    row.get::<_, i64>(3)
                        .map_err(|_| "source_unavailable".to_owned())?,
                )
                .map_err(|_| "source_unavailable".to_owned())?;
                report.evidence = true;
                increment(&mut report.token_observations, coverage_name(&coverage));
                report.freshest_at_ms = Some(report.freshest_at_ms.unwrap_or(0).max(observed));
                let token_count = row
                    .get::<_, Option<i64>>(1)
                    .map_err(|_| "source_unavailable".to_owned())?
                    .and_then(|value| u64::try_from(value).ok());
                let Some(token_count) = token_count else {
                    continue;
                };
                *report.tokens.entry(category.clone()).or_default() = report
                    .tokens
                    .get(&category)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(token_count);
                if category != "total_tokens" {
                    continue;
                }
                if let Some(index) = bucket_index(observed, start_ms, bucket_ms, bucket_count) {
                    report.token_buckets_observed[index] = true;
                    *report.phase_series[index]
                        .entry(operation.phase.clone())
                        .or_default() = report.phase_series[index]
                        .get(&operation.phase)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(token_count);
                    report.bucket_coverage[index] = if coverage == CoverageState::Complete
                        && report.bucket_coverage[index] != CoverageState::Partial
                    {
                        CoverageState::Complete
                    } else {
                        CoverageState::Partial
                    };
                }
                increment_pair(
                    &mut report.activities,
                    (&operation.phase, &operation.activity),
                    token_count,
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_coverage(
    connection: &Connection,
    report: &mut SourceReport,
    operations: &HashMap<String, Operation>,
    start_ms: u64,
    end_ms: u64,
    bucket_ms: u64,
    bucket_count: usize,
) -> Result<(), String> {
    let mut operation_ids = operations.keys().cloned().collect::<Vec<_>>();
    operation_ids.sort();
    for identifiers in operation_ids.chunks(SQLITE_VARIABLE_CHUNK) {
        let mut values = identifiers
            .iter()
            .cloned()
            .map(SqlValue::Text)
            .collect::<Vec<_>>();
        values.push(SqlValue::Integer(i64_value(start_ms)?));
        values.push(SqlValue::Integer(i64_value(end_ms)?));
        let sql = format!(
            "SELECT operation_id,coverage_state,occurred_at_ms FROM coverage_events WHERE operation_id IN ({}) AND occurred_at_ms>=? AND occurred_at_ms<?",
            placeholders(identifiers.len())
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| "source_unavailable".to_owned())?;
        let mut rows = statement
            .query(params_from_iter(values))
            .map_err(|_| "source_unavailable".to_owned())?;
        while let Some(row) = rows.next().map_err(|_| "source_unavailable".to_owned())? {
            let coverage = safe_coverage(
                &row.get::<_, String>(1)
                    .map_err(|_| "source_unavailable".to_owned())?,
            );
            let at = u64::try_from(
                row.get::<_, i64>(2)
                    .map_err(|_| "source_unavailable".to_owned())?,
            )
            .map_err(|_| "source_unavailable".to_owned())?;
            increment(&mut report.coverage_events, coverage_name(&coverage));
            report.evidence = true;
            if coverage != CoverageState::Complete
                && let Some(index) = bucket_index(at, start_ms, bucket_ms, bucket_count)
            {
                report.bucket_coverage[index] = CoverageState::Partial;
            }
        }
    }
    Ok(())
}

fn add_intervals(
    connection: &Connection,
    report: &mut SourceReport,
    operations: &HashMap<String, Operation>,
    start_ms: u64,
    end_ms: u64,
) -> Result<(), String> {
    let mut waits: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut unknown_waits = BTreeSet::new();
    let mut operation_ids = operations.keys().cloned().collect::<Vec<_>>();
    operation_ids.sort();
    for identifiers in operation_ids.chunks(SQLITE_VARIABLE_CHUNK) {
        let sql = format!(
            "SELECT span.operation_id,span.started_at_ms,ended.occurred_at_ms FROM activity_spans span LEFT JOIN activity_span_events ended ON ended.activity_span_id=span.id AND ended.event_kind='ended' WHERE span.operation_id IN ({})",
            placeholders(identifiers.len())
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|_| "source_unavailable".to_owned())?;
        let mut rows = statement
            .query(params_from_iter(identifiers.iter().cloned()))
            .map_err(|_| "source_unavailable".to_owned())?;
        while let Some(row) = rows.next().map_err(|_| "source_unavailable".to_owned())? {
            let operation = row
                .get::<_, String>(0)
                .map_err(|_| "source_unavailable".to_owned())?;
            let started = u64::try_from(
                row.get::<_, i64>(1)
                    .map_err(|_| "source_unavailable".to_owned())?,
            )
            .map_err(|_| "source_unavailable".to_owned())?;
            let ended = row
                .get::<_, Option<i64>>(2)
                .map_err(|_| "source_unavailable".to_owned())?
                .and_then(|value| u64::try_from(value).ok());
            if let Some(interval) = clip_interval(started, ended, start_ms, end_ms) {
                waits.entry(operation).or_default().push(interval);
            } else {
                unknown_waits.insert(operation);
            }
        }
    }
    for operation in operations.values() {
        let interval = clip_interval(
            operation.started_at_ms,
            operation.finished_at_ms,
            start_ms,
            end_ms,
        );
        let active = !matches!(
            operation.activity_state.as_str(),
            "external_wait" | "user_wait" | "blocked_wait"
        );
        let Some(interval) = interval else {
            report.execution_unknown = report.execution_unknown.saturating_add(1);
            increment(&mut report.phase_unknown, &operation.phase);
            if operation.kind == "model_request" {
                report.request_unknown = report.request_unknown.saturating_add(1);
            }
            if active {
                report.agent_unknown = report.agent_unknown.saturating_add(1);
            }
            continue;
        };
        report.execution_intervals.push(interval);
        report
            .phase_intervals
            .entry(operation.phase.clone())
            .or_default()
            .push(interval);
        if operation.kind == "model_request" {
            report.request_intervals.push(interval);
        }
        if !active {
            continue;
        }
        if unknown_waits.contains(&operation.id) || operation.agent_id.is_none() {
            report.agent_unknown = report.agent_unknown.saturating_add(1);
        } else {
            report
                .agent_intervals
                .entry(operation.agent_id.clone().expect("checked above"))
                .or_default()
                .extend(subtract(
                    interval,
                    waits.get(&operation.id).map(Vec::as_slice).unwrap_or(&[]),
                ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn combine(
    repository: &RepositoryRecord,
    range: UsageRange,
    now_ms: u64,
    start_ms: u64,
    bucket_ms: u64,
    bucket_count: usize,
    reports: &[(u32, SourceReport)],
    failures: BTreeMap<String, u64>,
    configured: usize,
) -> UsageRepository {
    let mut tokens = BTreeMap::new();
    let mut token_coverage = BTreeMap::new();
    let mut coverage_events = BTreeMap::new();
    let mut series = vec![BTreeMap::new(); bucket_count];
    let mut token_buckets_observed = vec![false; bucket_count];
    let mut bucket_coverage = vec![CoverageState::Unobserved; bucket_count];
    let mut activities = BTreeMap::new();
    let mut activity_operations = BTreeMap::new();
    let mut activity_provenance = BTreeMap::new();
    let mut outcomes = BTreeMap::new();
    let mut families = BTreeMap::new();
    let mut request_intervals = Vec::new();
    let mut execution_intervals = Vec::new();
    let mut agent_intervals: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut phase_intervals: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut phase_unknown = BTreeMap::new();
    let mut request_unknown = 0_u64;
    let mut execution_unknown = 0_u64;
    let mut agent_unknown = 0_u64;
    let mut operation_count = 0_u64;
    let mut model_requests = 0_u64;
    let mut tool_calls = 0_u64;
    let mut freshest = None;
    let mut contributed = 0_usize;
    let mut report_has_gaps = false;
    let mut schemas = BTreeSet::new();
    let mut taxonomies = BTreeSet::new();
    for (uid, report) in reports {
        schemas.insert(report.database_schema);
        taxonomies.insert(report.taxonomy_version);
        merge_counts(&mut tokens, &report.tokens);
        merge_counts(&mut token_coverage, &report.token_observations);
        merge_counts(&mut coverage_events, &report.coverage_events);
        for (index, values) in report.phase_series.iter().enumerate() {
            merge_counts(&mut series[index], values);
            let observed = report
                .token_buckets_observed
                .get(index)
                .copied()
                .unwrap_or(!values.is_empty());
            if observed {
                token_buckets_observed[index] = true;
            }
            match report.bucket_coverage[index] {
                CoverageState::Partial => bucket_coverage[index] = CoverageState::Partial,
                CoverageState::Complete if bucket_coverage[index] == CoverageState::Unobserved => {
                    bucket_coverage[index] = CoverageState::Complete;
                }
                _ => {}
            }
        }
        merge_pair_counts(&mut activities, &report.activities);
        merge_pair_counts(&mut activity_operations, &report.activity_operations);
        merge_triple_counts(&mut activity_provenance, &report.activity_provenance);
        merge_counts(&mut outcomes, &report.tool_outcomes);
        merge_counts(&mut families, &report.tool_families);
        request_intervals.extend_from_slice(&report.request_intervals);
        execution_intervals.extend_from_slice(&report.execution_intervals);
        for (agent, intervals) in &report.agent_intervals {
            agent_intervals
                .entry(format!("{uid}:{agent}"))
                .or_default()
                .extend_from_slice(intervals);
        }
        for (phase, intervals) in &report.phase_intervals {
            phase_intervals
                .entry(phase.clone())
                .or_default()
                .extend_from_slice(intervals);
        }
        request_unknown = request_unknown.saturating_add(report.request_unknown);
        execution_unknown = execution_unknown.saturating_add(report.execution_unknown);
        agent_unknown = agent_unknown.saturating_add(report.agent_unknown);
        merge_counts(&mut phase_unknown, &report.phase_unknown);
        operation_count = operation_count.saturating_add(report.operation_count);
        model_requests = model_requests.saturating_add(report.model_request_count);
        tool_calls = tool_calls.saturating_add(report.tool_count);
        freshest = Some(
            freshest
                .unwrap_or(0)
                .max(report.freshest_at_ms.unwrap_or(0)),
        );
        contributed += usize::from(report.evidence);
        report_has_gaps |= report.evidence
            && (report.token_observations.is_empty()
                || report.execution_unknown > 0
                || report.request_unknown > 0
                || report.agent_unknown > 0);
    }
    let has_gaps = !failures.is_empty()
        || token_coverage.keys().any(|state| state != "complete")
        || coverage_events.keys().any(|state| state != "complete")
        || report_has_gaps;
    let coverage_state = if reports.is_empty() {
        CoverageState::Unavailable
    } else if contributed == 0 {
        CoverageState::Unobserved
    } else if reports.len() == configured && !has_gaps {
        CoverageState::Complete
    } else {
        CoverageState::Partial
    };
    if coverage_state != CoverageState::Complete {
        for coverage in &mut bucket_coverage {
            if *coverage != CoverageState::Unobserved || !failures.is_empty() {
                *coverage = CoverageState::Partial;
            }
        }
    }
    let total_tokens = (!token_coverage.is_empty())
        .then(|| tokens.get("total_tokens").copied())
        .flatten();
    let mut activity_rows = activities
        .keys()
        .chain(activity_operations.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|(phase, activity)| {
            let value = activities
                .get(&(phase.clone(), activity.clone()))
                .copied()
                .unwrap_or(0);
            UsageActivity {
                phase: phase.clone(),
                activity: activity.clone(),
                total_tokens: value,
                share: total_tokens
                    .filter(|total| *total > 0)
                    .map(|total| value as f64 / total as f64),
                operations: activity_operations
                    .get(&(phase.clone(), activity.clone()))
                    .copied()
                    .unwrap_or(0),
                provenance: activity_provenance
                    .iter()
                    .filter(|((p, a, _), _)| p == &phase && a == &activity)
                    .map(|((_, _, provenance), count)| (provenance.clone(), *count))
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    activity_rows.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then_with(|| left.phase.cmp(&right.phase))
            .then_with(|| left.activity.cmp(&right.activity))
    });
    let request_ms = request_intervals
        .iter()
        .map(|(_, end)| *end)
        .max()
        .zip(request_intervals.iter().map(|(start, _)| *start).min())
        .map_or(0, |(end, start)| end.saturating_sub(start));
    if contributed > 0 && request_intervals.is_empty() && request_unknown == 0 {
        request_unknown = 1;
    }
    let execution_ms = union_ms(&execution_intervals);
    let agent_ms = agent_intervals
        .values()
        .map(|intervals| union_ms(intervals))
        .fold(0_u64, u64::saturating_add);
    let phases = phase_intervals
        .keys()
        .chain(phase_unknown.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|phase| PhaseTime {
            measured_ms: union_ms(
                phase_intervals
                    .get(&phase)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            ),
            unknown_intervals: phase_unknown.get(&phase).copied().unwrap_or(0),
            phase,
        })
        .collect();
    let points = series
        .into_iter()
        .enumerate()
        .map(|(index, values)| {
            let phases = PHASES
                .iter()
                .map(|phase| ((*phase).into(), values.get(*phase).copied().unwrap_or(0)))
                .collect::<BTreeMap<_, _>>();
            UsageSeriesPoint {
                bucket_start_ms: start_ms.saturating_add(bucket_ms.saturating_mul(index as u64)),
                bucket_end_ms: start_ms.saturating_add(bucket_ms.saturating_mul(index as u64 + 1)),
                coverage: bucket_coverage[index].clone(),
                total_tokens: token_buckets_observed[index].then(|| phases.values().copied().sum()),
                phases,
            }
        })
        .collect();
    let mut family_rows = families
        .into_iter()
        .map(|(family, count)| ToolFamily { family, count })
        .collect::<Vec<_>>();
    family_rows.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.family.cmp(&right.family))
    });
    family_rows.truncate(12);
    UsageRepository {
        repository_id: repository.repository_id.clone(),
        display_name: repository.display_name.clone(),
        range,
        generated_at_ms: now_ms,
        coverage: UsageCoverage {
            snapshot: None,
            state: coverage_state.clone(),
            has_gaps: coverage_state != CoverageState::Complete,
            configured_collectors: u32::try_from(configured).unwrap_or(u32::MAX),
            available_collectors: u32::try_from(reports.len()).unwrap_or(u32::MAX),
            contributing_collectors: u32::try_from(contributed).unwrap_or(u32::MAX),
            freshest_at_ms: freshest.filter(|value| *value > 0),
            events: coverage_events,
            token_observations: token_coverage,
            unavailable_reasons: failures,
            database_schemas: schemas.into_iter().collect(),
            taxonomy_versions: taxonomies.into_iter().collect(),
        },
        totals: UsageTotals {
            total_tokens,
            input_tokens: tokens.get("input_tokens").copied(),
            cached_input_tokens: tokens.get("input_tokens_details.cached_tokens").copied(),
            output_tokens: tokens.get("output_tokens").copied(),
            reasoning_tokens: tokens
                .get("output_tokens_details.reasoning_tokens")
                .copied(),
            model_requests,
            tool_calls,
            operations: operation_count,
        },
        series: points,
        activities: activity_rows,
        time: UsageTime {
            request_to_delivery: MeasuredTime {
                measured_ms: request_ms,
                unknown_intervals: request_unknown,
            },
            execution_wall: MeasuredTime {
                measured_ms: execution_ms,
                unknown_intervals: execution_unknown,
            },
            summed_agent_active: MeasuredTime {
                measured_ms: agent_ms,
                unknown_intervals: agent_unknown,
            },
            phases,
        },
        tools: UsageTools {
            outcomes: outcomes
                .into_iter()
                .map(|(outcome, count)| ToolOutcome { outcome, count })
                .collect(),
            families: family_rows,
        },
        semantics: UsageSemantics {
            tokens: "provider total_tokens only; cached input and reasoning are subsets".into(),
            time: "wall, execution, phase, agent, and tool durations are separate".into(),
            coverage: "partial and unavailable collectors never contribute synthetic zeroes".into(),
        },
    }
}

impl RepositoryProbe for HostRepositoryProbe {
    fn probe(
        &self,
        source: &CodexUsageSource,
        repository: &Path,
        now_ms: u64,
    ) -> Result<(String, u32, u32), String> {
        match self.probe_format(source, repository, now_ms, true) {
            Err(reason) if reason == "identity_unsupported" => {
                self.probe_format(source, repository, now_ms, false)
            }
            result => result,
        }
    }
}

impl HostRepositoryProbe {
    fn probe_format(
        &self,
        source: &CodexUsageSource,
        repository: &Path,
        now_ms: u64,
        identity_only: bool,
    ) -> Result<(String, u32, u32), String> {
        if !source.executable.is_absolute()
            || !source.executable.is_file()
            || !source.codex_home.is_absolute()
            || !source.codex_home.is_dir()
            || !repository.is_absolute()
            || !repository.is_dir()
        {
            return Err("source_unavailable".into());
        }
        let user = user_record(source.uid).map_err(|_| "source_unavailable")?;
        let effective = rustix::process::geteuid().as_raw();
        if effective != 0 && effective != source.uid {
            return Err("source_unavailable".into());
        }
        let mut command = if effective == 0 && source.uid != 0 {
            let setpriv = ["/usr/bin/setpriv", "/bin/setpriv"]
                .into_iter()
                .map(Path::new)
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| "source_unavailable".to_owned())?;
            let mut command = Command::new(setpriv);
            command
                .arg(format!("--reuid={}", source.uid))
                .arg(format!("--regid={}", user.gid))
                .arg("--init-groups")
                .arg("--")
                .arg(&source.executable);
            command
        } else {
            Command::new(&source.executable)
        };
        if identity_only {
            command.args(["usage", "--json", "repo", "current", "--identity-only"]);
        } else {
            command.args([
                "usage",
                "--json",
                "--since",
                &now_ms.to_string(),
                "repo",
                "current",
            ]);
        }
        command
            .current_dir(repository)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", user.home)
            .env("USER", &user.name)
            .env("LOGNAME", user.name)
            .env("CODEX_HOME", &source.codex_home)
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_probe(command)?;
        if !output.status.success() || output.stdout_truncated {
            if !output.stderr_truncated {
                if serde_json::from_slice::<serde_json::Value>(&output.stderr)
                    .ok()
                    .and_then(|document| {
                        document
                            .get("error")?
                            .get("code")?
                            .as_str()
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some("not_found")
                {
                    return Err("mapping_unavailable".into());
                }
                let message = String::from_utf8_lossy(&output.stderr);
                if identity_only && message.contains("unexpected argument '--identity-only'") {
                    return Err("identity_unsupported".into());
                }
            }
            return Err("source_unavailable".into());
        }
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|_| "source_unavailable")?;
        let key = document
            .get("scope")
            .and_then(|scope| scope.get("id"))
            .and_then(serde_json::Value::as_str)
            .filter(|key| valid_repository_key(key))
            .ok_or_else(|| "source_unavailable".to_owned())?;
        let scope = document
            .get("scope")
            .and_then(|scope| scope.get("type"))
            .and_then(serde_json::Value::as_str);
        let schema = document
            .get("databaseSchemaVersion")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| "source_unavailable".to_owned())?;
        let taxonomy = document
            .get("taxonomyVersion")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| "source_unavailable".to_owned())?;
        if document
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
            || document.get("kind").and_then(serde_json::Value::as_str)
                != Some(if identity_only {
                    "usageRepositoryIdentity"
                } else {
                    "usageSummary"
                })
            || scope != Some("repository")
        {
            return Err("source_unavailable".into());
        }
        Ok((key.into(), schema, taxonomy))
    }
}

struct UserRecord {
    name: OsString,
    home: OsString,
    gid: u32,
}

fn user_record(uid: u32) -> io::Result<UserRecord> {
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: A zeroed passwd value is a valid getpwuid_r output buffer.
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: Every pointer names live writable storage for the stated length.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut record,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    if result.is_null() || record.pw_name.is_null() || record.pw_dir.is_null() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "user not found"));
    }
    // SAFETY: Successful getpwuid_r returned NUL-terminated values backed by
    // `buffer`, which remains live through these copies.
    let name = OsString::from_vec(
        unsafe { CStr::from_ptr(record.pw_name) }
            .to_bytes()
            .to_vec(),
    );
    // SAFETY: Same getpwuid_r contract as pw_name.
    let home = OsString::from_vec(unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes().to_vec());
    Ok(UserRecord {
        name,
        home,
        gid: record.pw_gid,
    })
}

struct ProbeOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stdout_truncated: bool,
    stderr: Vec<u8>,
    stderr_truncated: bool,
}

fn run_probe(mut command: Command) -> Result<ProbeOutput, String> {
    let mut child = command.spawn().map_err(|_| "source_unavailable")?;
    let stdout = child.stdout.take().ok_or("source_unavailable")?;
    let stderr = child.stderr.take().ok_or("source_unavailable")?;
    let stdout = thread::spawn(move || read_capture(stdout, SOURCE_OUTPUT_BYTES));
    let stderr = thread::spawn(move || read_capture(stderr, SOURCE_OUTPUT_BYTES));
    let deadline = Instant::now() + SOURCE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| "source_unavailable")? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            return Err("source_unavailable".into());
        }
        thread::sleep(PROCESS_POLL.min(deadline.saturating_duration_since(now)));
    };
    let (stdout, stdout_truncated) = stdout
        .join()
        .map_err(|_| "source_unavailable")?
        .map_err(|_| "source_unavailable")?;
    let (stderr, stderr_truncated) = stderr
        .join()
        .map_err(|_| "source_unavailable")?
        .map_err(|_| "source_unavailable")?;
    Ok(ProbeOutput {
        status,
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
    })
}

fn read_capture(mut source: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok((output, truncated));
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
}

fn usage_window(range: &UsageRange, now_ms: u64) -> (u64, u64, u64, usize) {
    let (bucket, count) = match range {
        UsageRange::Hours24 => (60 * 60 * 1_000, 24),
        UsageRange::Days7 => (6 * 60 * 60 * 1_000, 28),
        UsageRange::Days30 => (24 * 60 * 60 * 1_000, 30),
    };
    let aligned_end = now_ms.div_ceil(bucket).saturating_mul(bucket);
    (
        aligned_end.saturating_sub(bucket.saturating_mul(count as u64)),
        now_ms,
        bucket,
        count,
    )
}

fn range_name(range: &UsageRange) -> &'static str {
    match range {
        UsageRange::Hours24 => "24h",
        UsageRange::Days7 => "7d",
        UsageRange::Days30 => "30d",
    }
}

fn bucket_index(at: u64, start: u64, bucket: u64, count: usize) -> Option<usize> {
    let index = at.checked_sub(start)? / bucket;
    usize::try_from(index).ok().filter(|index| *index < count)
}

fn clip_interval(
    start: u64,
    end: Option<u64>,
    range_start: u64,
    range_end: u64,
) -> Option<(u64, u64)> {
    let end = end.filter(|end| *end >= start)?;
    let clipped = (start.max(range_start), end.min(range_end));
    (clipped.0 <= clipped.1).then_some(clipped)
}

fn union_ms(intervals: &[(u64, u64)]) -> u64 {
    let mut intervals = intervals.to_vec();
    intervals.sort_unstable();
    let Some((mut start, mut end)) = intervals.first().copied() else {
        return 0;
    };
    let mut total = 0_u64;
    for (next_start, next_end) in intervals.into_iter().skip(1) {
        if next_start <= end {
            end = end.max(next_end);
        } else {
            total = total.saturating_add(end.saturating_sub(start));
            start = next_start;
            end = next_end;
        }
    }
    total.saturating_add(end.saturating_sub(start))
}

fn subtract(base: (u64, u64), exclusions: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut relevant = exclusions
        .iter()
        .map(|(start, end)| (base.0.max(*start), base.1.min(*end)))
        .filter(|(start, end)| start <= end)
        .collect::<Vec<_>>();
    relevant.sort_unstable();
    let mut result = Vec::new();
    let mut cursor = base.0;
    for (start, end) in relevant {
        if start > cursor {
            result.push((cursor, start));
        }
        cursor = cursor.max(end);
    }
    if cursor < base.1 {
        result.push((cursor, base.1));
    }
    result
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

fn i64_value(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "source_unavailable".into())
}

fn valid_repository_key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_phase(value: &str) -> String {
    if PHASES.contains(&value) {
        value.into()
    } else {
        "unattributed".into()
    }
}

fn safe_label(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        value.into()
    } else {
        "unknown".into()
    }
}

fn safe_coverage(value: &str) -> CoverageState {
    match value {
        "complete" => CoverageState::Complete,
        "unavailable" => CoverageState::Unavailable,
        "unobserved" => CoverageState::Unobserved,
        _ => CoverageState::Partial,
    }
}

fn coverage_name(value: &CoverageState) -> &'static str {
    match value {
        CoverageState::Complete => "complete",
        CoverageState::Partial => "partial",
        CoverageState::Unavailable => "unavailable",
        CoverageState::Unobserved => "unobserved",
    }
}

fn tool_outcome(value: Option<&str>) -> &'static str {
    match value {
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("denied") => "rejected",
        Some("cancelled" | "superseded" | "interrupted") => "interrupted",
        _ => "unknown",
    }
}

fn increment(values: &mut BTreeMap<String, u64>, name: &str) {
    let value = values.entry(name.into()).or_default();
    *value = value.saturating_add(1);
}

fn increment_pair(values: &mut BTreeMap<(String, String), u64>, key: (&str, &str), amount: u64) {
    let value = values.entry((key.0.into(), key.1.into())).or_default();
    *value = value.saturating_add(amount);
}

fn increment_triple(
    values: &mut BTreeMap<(String, String, String), u64>,
    key: (&str, &str, &str),
    amount: u64,
) {
    let value = values
        .entry((key.0.into(), key.1.into(), key.2.into()))
        .or_default();
    *value = value.saturating_add(amount);
}

fn merge_counts(target: &mut BTreeMap<String, u64>, source: &BTreeMap<String, u64>) {
    for (key, amount) in source {
        let value = target.entry(key.clone()).or_default();
        *value = value.saturating_add(*amount);
    }
}

fn merge_pair_counts(
    target: &mut BTreeMap<(String, String), u64>,
    source: &BTreeMap<(String, String), u64>,
) {
    for (key, amount) in source {
        let value = target.entry(key.clone()).or_default();
        *value = value.saturating_add(*amount);
    }
}

fn merge_triple_counts(
    target: &mut BTreeMap<(String, String, String), u64>,
    source: &BTreeMap<(String, String, String), u64>,
) {
    for (key, amount) in source {
        let value = target.entry(key.clone()).or_default();
        *value = value.saturating_add(*amount);
    }
}

fn source_error(reason: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, reason.into())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "usage link storage failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::platform::FixedClock;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tempfile::tempdir;
    use time::macros::datetime;

    struct FixtureProbe;
    impl RepositoryProbe for FixtureProbe {
        fn probe(
            &self,
            source: &CodexUsageSource,
            _repository: &Path,
            _now_ms: u64,
        ) -> Result<(String, u32, u32), String> {
            let connection = open_source(source)?;
            Ok((
                "b".repeat(64),
                maximum_version(&connection, "_sqlx_migrations")?,
                1,
            ))
        }
    }

    struct BlockingProbe {
        entered: AtomicU64,
        released: AtomicBool,
    }

    impl RepositoryProbe for BlockingProbe {
        fn probe(
            &self,
            _source: &CodexUsageSource,
            _repository: &Path,
            _now_ms: u64,
        ) -> Result<(String, u32, u32), String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            while !self.released.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            Err("source_unavailable".into())
        }
    }

    pub(crate) fn source_database(codex_home: &Path, schema: i64) -> (String, u64) {
        let usage = codex_home.join("usage");
        std::fs::create_dir_all(&usage).unwrap();
        let connection = Connection::open(usage.join("usage.sqlite3")).unwrap();
        connection.execute_batch(
            "CREATE TABLE _sqlx_migrations(version INTEGER);
             CREATE TABLE taxonomy_versions(version INTEGER);
             CREATE TABLE repository_merge_events(source_repository_id TEXT,target_repository_id TEXT);
             CREATE TABLE repositories(id TEXT PRIMARY KEY);
             CREATE TABLE operations(id TEXT PRIMARY KEY,operation_kind TEXT,agent_id TEXT,started_at_ms INTEGER,phase TEXT,activity TEXT,activity_state TEXT,attribution_provenance TEXT);
             CREATE TABLE operation_events(operation_id TEXT,terminal INTEGER,occurred_at_ms INTEGER,event_kind TEXT);
             CREATE TABLE tool_invocations(id TEXT PRIMARY KEY,operation_id TEXT,operation_family TEXT);
             CREATE TABLE model_requests(id TEXT PRIMARY KEY,operation_id TEXT);
             CREATE TABLE repository_attributions(operation_id TEXT,repository_id TEXT);
             CREATE TABLE token_observations(category_path TEXT,token_count INTEGER,coverage_state TEXT,observed_at_ms INTEGER,model_request_id TEXT,tool_invocation_id TEXT,repository_bucket TEXT,measurement_provenance TEXT);
             CREATE TABLE coverage_events(operation_id TEXT,coverage_state TEXT,occurred_at_ms INTEGER);
             CREATE TABLE activity_spans(id TEXT PRIMARY KEY,operation_id TEXT,started_at_ms INTEGER);
             CREATE TABLE activity_span_events(activity_span_id TEXT,event_kind TEXT,occurred_at_ms INTEGER);
             CREATE TABLE effective_classification_events(operation_id TEXT,phase TEXT,activity TEXT,activity_state TEXT,provenance TEXT);
             CREATE INDEX token_model_lookup ON token_observations(model_request_id,repository_bucket,category_path,observed_at_ms);
             CREATE INDEX token_tool_lookup ON token_observations(tool_invocation_id,repository_bucket,category_path,observed_at_ms);
             CREATE INDEX coverage_operation_lookup ON coverage_events(operation_id,occurred_at_ms);
             CREATE INDEX effective_operation_lookup ON effective_classification_events(operation_id);
             CREATE INDEX span_operation_lookup ON activity_spans(operation_id);",
        )
        .unwrap();
        connection
            .execute("INSERT INTO _sqlx_migrations VALUES(?1)", [schema])
            .unwrap();
        connection
            .execute("INSERT INTO taxonomy_versions VALUES(1)", [])
            .unwrap();
        let source = "a".repeat(64);
        let canonical = "b".repeat(64);
        connection
            .execute(
                "INSERT INTO repository_merge_events VALUES(?1,?2)",
                rusqlite::params![source, canonical],
            )
            .unwrap();
        connection
            .execute("INSERT INTO repositories VALUES(?1)", [&source])
            .unwrap();
        connection
            .execute("INSERT INTO repositories VALUES(?1)", [&canonical])
            .unwrap();
        let now_ms = 1_788_000_000_000_u64;
        let start = i64::try_from(now_ms - 60_000).unwrap();
        connection.execute(
            "INSERT INTO operations VALUES('model-op','model_request','agent-private',?1,'implementation','coding','model_active','agent_declared')",
            [start],
        ).unwrap();
        connection
            .execute(
                "INSERT INTO operation_events VALUES('model-op',1,?1,'completed')",
                [start + 10_000],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO model_requests VALUES('request-private','model-op')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO repository_attributions VALUES('model-op',?1)",
                [&source],
            )
            .unwrap();
        connection.execute("INSERT INTO effective_classification_events VALUES('model-op','implementation','coding','model_active','agent_declared')",[]).unwrap();
        connection.execute(
            "INSERT INTO operations VALUES('tool-op','local_tool','agent-private',?1,'testing','integration_testing','tool_active','agent_declared')",
            [start + 4_000],
        ).unwrap();
        connection
            .execute(
                "INSERT INTO operation_events VALUES('tool-op',1,?1,'completed')",
                [start + 8_000],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tool_invocations VALUES('tool-private','tool-op','execution')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO repository_attributions VALUES('tool-op',?1)",
                [&source],
            )
            .unwrap();
        connection.execute("INSERT INTO effective_classification_events VALUES('tool-op','testing','integration_testing','tool_active','agent_declared')",[]).unwrap();
        for (category, count) in [
            ("total_tokens", 100),
            ("input_tokens", 80),
            ("input_tokens_details.cached_tokens", 50),
            ("output_tokens", 20),
            ("output_tokens_details.reasoning_tokens", 10),
        ] {
            connection.execute(
                "INSERT INTO token_observations VALUES(?1,?2,'complete',?3,'request-private',NULL,?4,'provider_reported')",
                rusqlite::params![category,count,start+10_000,source],
            ).unwrap();
        }
        for operation in ["model-op", "tool-op"] {
            connection
                .execute(
                    "INSERT INTO coverage_events VALUES(?1,'complete',?2)",
                    rusqlite::params![operation, start + 9_000],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO activity_spans VALUES('wait-private','model-op',?1)",
                [start + 2_000],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO activity_span_events VALUES('wait-private','ended',?1)",
                [start + 3_000],
            )
            .unwrap();
        if matches!(schema, 5 | 6) {
            connection.execute_batch(
                "CREATE TABLE model_request_context_sources (
                    model_request_id TEXT PRIMARY KEY NOT NULL REFERENCES model_requests(id),
                    policy_estimated_tokens INTEGER NOT NULL CHECK (policy_estimated_tokens >= 0),
                    conversation_estimated_tokens INTEGER NOT NULL CHECK (conversation_estimated_tokens >= 0),
                    tool_output_estimated_tokens INTEGER NOT NULL CHECK (tool_output_estimated_tokens >= 0),
                    estimator TEXT NOT NULL CHECK (estimator = 'approx_model_visible_v1'),
                    observed_at_ms INTEGER NOT NULL
                 ) STRICT;
                 ALTER TABLE tool_invocations ADD COLUMN execution_group_id TEXT;
                 ALTER TABLE tool_invocations ADD COLUMN execution_role TEXT NOT NULL DEFAULT 'standalone'
                    CHECK (execution_role IN ('standalone', 'wrapper', 'nested'));
                 INSERT INTO model_request_context_sources VALUES
                    ('request-private',9000,8000,7000,'approx_model_visible_v1',0);",
            ).unwrap();
        }
        if schema == 6 {
            connection
                .execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY);")
                .unwrap();
            connection
                .execute_batch(include_str!(
                    "../tests/fixtures/codex-usage-0006-work-bindings.sql"
                ))
                .unwrap();
        }
        drop(connection);
        (canonical, now_ms)
    }

    fn config(root: &Path, codex_home: PathBuf) -> Config {
        Config {
            socket_path: root.join("daemon.sock"),
            state_dir: root.join("state"),
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "clients".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: root.join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: vec![CodexUsageSource {
                uid: rustix::process::getuid().as_raw(),
                codex_home,
                executable: root.join("codex"),
            }],
        }
    }

    #[test]
    fn repository_usage_is_additive_complete_and_excludes_private_source_identity() {
        assert_repository_usage(4);
    }

    #[test]
    fn schema_five_usage_preserves_provider_measurements_and_privacy() {
        assert_repository_usage(5);
    }

    #[test]
    fn work_context_schema_six_addition_preserves_measurements_and_privacy() {
        assert_repository_usage(6);
    }

    #[test]
    fn review_usage_reads_exact_indexed_window_without_persistent_measurements() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        let (canonical, now_ms) = source_database(&codex_home, 5);
        let authority = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let repository = RepositoryRecord {
            repository_id: "project-alpha".into(),
            display_name: "Project Alpha".into(),
            root_path: temporary.path().join("repo"),
        };
        let usage = CodexUsage::with_probe(
            config(temporary.path(), codex_home.clone()),
            authority.clone(),
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(FixtureProbe),
        );
        let uid = rustix::process::getuid().as_raw();
        authority.call(move |connection| {
            connection.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('project-alpha','/fixture','Project Alpha','t',1,'t')", [])?;
            connection.execute("INSERT INTO codex_usage_repository_links VALUES(?1,'project-alpha',?2,5,1,'t')", rusqlite::params![uid,canonical])?;
            Ok(())
        }).unwrap();
        let service = UsageService {
            registry: Registry::new(authority.clone()),
            usage,
        };
        let included = service
            .review_window(
                &repository,
                now_ms - 60_000,
                now_ms - 49_999,
                Instant::now() + QUERY_TIMEOUT,
            )
            .unwrap();
        let excluded = service
            .review_window(
                &repository,
                now_ms - 49_999,
                now_ms,
                Instant::now() + QUERY_TIMEOUT,
            )
            .unwrap();
        let broad = service
            .review_window(&repository, 0, now_ms, Instant::now() + QUERY_TIMEOUT)
            .unwrap();
        assert_eq!(broad.totals, included.totals);
        assert_eq!(included.totals.total_tokens, Some(100));
        assert_eq!(excluded.totals.model_requests, 0);
        let source = Connection::open(codex_home.join("usage/usage.sqlite3")).unwrap();
        assert_eq!(source.query_row("SELECT SUM(token_count) FROM token_observations WHERE category_path='total_tokens'", [], |row| row.get::<_, i64>(0)).unwrap(), 100);
        let mirrored: i32 = authority
            .call(|connection| {
                Ok(connection
                    .query_row("SELECT COUNT(*) FROM review_records", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(mirrored, 0);
    }

    #[test]
    fn review_usage_exhaustion_and_missing_mapping_are_explicit_without_probes() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        let (_, now_ms) = source_database(&codex_home, 5);
        let authority = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let repository = RepositoryRecord {
            repository_id: "project-alpha".into(),
            display_name: "Project Alpha".into(),
            root_path: temporary.path().join("repo"),
        };
        let probe = Arc::new(BlockingProbe {
            entered: AtomicU64::new(0),
            released: AtomicBool::new(true),
        });
        let usage = CodexUsage::with_probe(
            config(temporary.path(), codex_home),
            authority,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            probe.clone(),
        );
        for (deadline, reason) in [
            (Instant::now(), "query_budget_exhausted"),
            (Instant::now() + QUERY_TIMEOUT, "mapping_pending"),
        ] {
            let report = usage
                .repository_window(
                    &repository,
                    UsageRange::Hours24,
                    now_ms,
                    0,
                    now_ms,
                    now_ms,
                    1,
                    false,
                    Some(deadline),
                )
                .unwrap();
            assert!(report.coverage.has_gaps);
            assert_eq!(
                report.coverage.unavailable_reasons,
                BTreeMap::from([(reason.to_owned(), 1)])
            );
            assert_eq!(report.totals.total_tokens, None);
        }
        assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn review_source_query_deadline_interrupts_sql_instead_of_scanning_without_a_limit() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        source_database(&codex_home, 5);
        let settings = config(temporary.path(), codex_home);
        let connection =
            open_source_until(&settings.codex_usage_sources[0], Instant::now()).unwrap();
        let result = connection.query_row("WITH RECURSIVE numbers(value) AS (VALUES(0) UNION ALL SELECT value+1 FROM numbers WHERE value<1000000) SELECT SUM(value) FROM numbers", [], |row| row.get::<_, i64>(0));
        assert!(
            matches!(result, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted)
        );
    }

    fn assert_repository_usage(schema: i64) {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        let (canonical, now_ms) = source_database(&codex_home, schema);
        let config = config(temporary.path(), codex_home.clone());
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let authority = Database::open(config.database_path()).unwrap();
        let repository = RepositoryRecord {
            repository_id: "r0123456789abcdef".into(),
            display_name: "Example".into(),
            root_path: temporary.path().join("repo"),
        };
        std::fs::create_dir(&repository.root_path).unwrap();
        let repository_id = repository.repository_id.clone();
        let canonical_for_db = "c".repeat(64);
        let source_connection = Connection::open(codex_home.join("usage/usage.sqlite3")).unwrap();
        source_connection
            .execute("INSERT INTO repositories VALUES(?1)", [&canonical_for_db])
            .unwrap();
        let uid = rustix::process::getuid().as_raw();
        authority.transaction(move |transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,'/repo','Example','t',1,'t')",[&repository_id])?;
            transaction.execute("INSERT INTO codex_usage_repository_links VALUES(?1,?2,?3,4,1,'t')",rusqlite::params![uid,repository_id,canonical_for_db])?;
            Ok(())
        }).unwrap();
        let usage = CodexUsage::with_probe(
            config,
            authority,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(FixtureProbe),
        );
        let report = usage
            .repository_at(&repository, UsageRange::Hours24, now_ms)
            .unwrap();
        assert_eq!(report.coverage.state, CoverageState::Complete);
        assert_eq!(report.totals.total_tokens, Some(100));
        assert_eq!(report.totals.cached_input_tokens, Some(50));
        assert_eq!(report.totals.model_requests, 1);
        assert_eq!(report.totals.tool_calls, 1);
        assert_eq!(report.time.request_to_delivery.measured_ms, 10_000);
        assert_eq!(report.time.execution_wall.measured_ms, 10_000);
        assert_eq!(report.time.summed_agent_active.measured_ms, 9_000);
        let observed = report
            .series
            .iter()
            .filter_map(|point| point.total_tokens)
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![100]);
        assert_eq!(
            report
                .series
                .iter()
                .filter(|point| point.total_tokens.is_none())
                .count(),
            report.series.len() - 1
        );
        let rendered = serde_json::to_string(&report).unwrap();
        for private in [
            codex_home.to_string_lossy().as_ref(),
            canonical.as_str(),
            "agent-private",
            "request-private",
            "tool-private",
            "wait-private",
        ] {
            assert!(!rendered.contains(private));
        }
    }

    #[test]
    fn repository_detail_uses_indexed_identifiers_at_production_scale() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        let (canonical, now_ms) = source_database(&codex_home, 4);
        let source_path = codex_home.join("usage/usage.sqlite3");
        let mut source = Connection::open(source_path).unwrap();
        let transaction = source.transaction().unwrap();
        {
            let mut token = transaction
                .prepare("INSERT INTO token_observations VALUES('total_tokens',1,'complete',?1,'unrelated',NULL,?2,'provider_reported')")
                .unwrap();
            let mut coverage = transaction
                .prepare("INSERT INTO coverage_events VALUES('unrelated','complete',?1)")
                .unwrap();
            let at = i64::try_from(now_ms - 30_000).unwrap();
            let bucket = "a".repeat(64);
            for _ in 0..100_000 {
                token.execute(rusqlite::params![at, bucket]).unwrap();
                coverage.execute([at]).unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(source);

        let config = config(temporary.path(), codex_home);
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let authority = Database::open(config.database_path()).unwrap();
        let repository = RepositoryRecord {
            repository_id: "r0123456789abcdef".into(),
            display_name: "Example".into(),
            root_path: temporary.path().join("repo"),
        };
        std::fs::create_dir(&repository.root_path).unwrap();
        let repository_id = repository.repository_id.clone();
        let uid = rustix::process::getuid().as_raw();
        authority
            .transaction(move |transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,'/repo','Example','t',1,'t')",[&repository_id])?;
                transaction.execute("INSERT INTO codex_usage_repository_links VALUES(?1,?2,?3,4,1,'t')",rusqlite::params![uid,repository_id,canonical])?;
                Ok(())
            })
            .unwrap();
        let usage = CodexUsage::with_probe(
            config,
            authority,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(FixtureProbe),
        );
        let detail = usage
            .repository_buckets(
                &repository,
                UsageRange::Hours24,
                60_000,
                2,
                now_ms,
                now_ms,
                true,
            )
            .unwrap();
        assert!(detail.coverage.snapshot.as_ref().unwrap().refreshing);
        usage.wait_for_refresh(Some(&repository.repository_id));
        let detail = usage
            .repository_buckets(
                &repository,
                UsageRange::Hours24,
                60_000,
                2,
                now_ms,
                now_ms,
                true,
            )
            .unwrap();
        assert_eq!(detail.totals.total_tokens, Some(100));
        assert_eq!(
            detail
                .series
                .iter()
                .map(|point| point.total_tokens)
                .collect::<Vec<_>>(),
            vec![None, Some(100)]
        );
    }

    #[test]
    fn partial_source_failure_keeps_missing_bucket_blank_and_real_zero_measured() {
        let report = SourceReport {
            database_schema: 4,
            taxonomy_version: 1,
            evidence: true,
            tokens: BTreeMap::from([("total_tokens".into(), 0)]),
            token_observations: BTreeMap::from([("complete".into(), 1)]),
            phase_series: vec![
                BTreeMap::new(),
                BTreeMap::from([("implementation".into(), 0)]),
            ],
            token_buckets_observed: vec![false, true],
            bucket_coverage: vec![CoverageState::Unobserved, CoverageState::Complete],
            ..SourceReport::default()
        };
        let combined = combine(
            &RepositoryRecord {
                repository_id: "r1".into(),
                display_name: "Example".into(),
                root_path: "/repo".into(),
            },
            UsageRange::Hours24,
            120_000,
            0,
            60_000,
            2,
            &[(1, report)],
            BTreeMap::from([("source_unavailable".into(), 1)]),
            2,
        );
        assert_eq!(combined.coverage.state, CoverageState::Partial);
        assert_eq!(
            combined
                .series
                .iter()
                .map(|point| point.coverage.clone())
                .collect::<Vec<_>>(),
            vec![CoverageState::Partial, CoverageState::Partial]
        );
        assert_eq!(
            combined
                .series
                .iter()
                .map(|point| point.total_tokens)
                .collect::<Vec<_>>(),
            vec![None, Some(0)]
        );
        assert_eq!(combined.totals.total_tokens, Some(0));
    }

    #[test]
    fn unsupported_and_unconfigured_collectors_are_unavailable_not_zero() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        let (canonical, now_ms) = source_database(&codex_home, 7);
        let mut config = config(temporary.path(), codex_home);
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let authority = Database::open(config.database_path()).unwrap();
        let repository = RepositoryRecord {
            repository_id: "r0123456789abcdef".into(),
            display_name: "Example".into(),
            root_path: temporary.path().join("repo"),
        };
        std::fs::create_dir(&repository.root_path).unwrap();
        let repository_id = repository.repository_id.clone();
        let uid = rustix::process::getuid().as_raw();
        authority.transaction(move |transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,'/repo','Example','t',1,'t')",[&repository_id])?;
            transaction.execute("INSERT INTO codex_usage_repository_links VALUES(?1,?2,?3,7,1,'t')",rusqlite::params![uid,repository_id,canonical])?;
            Ok(())
        }).unwrap();
        let usage = CodexUsage::with_probe(
            config.clone(),
            authority,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(FixtureProbe),
        );
        let report = usage
            .repository_at(&repository, UsageRange::Hours24, now_ms)
            .unwrap();
        assert_eq!(report.coverage.state, CoverageState::Unavailable);
        assert_eq!(report.coverage.unavailable_reasons["schema_unsupported"], 1);
        assert_eq!(report.totals.total_tokens, None);

        config.codex_usage_sources.clear();
        let empty = CodexUsage::with_probe(
            config,
            Database::open(temporary.path().join("empty.sqlite3")).unwrap(),
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(FixtureProbe),
        )
        .repository_at(&repository, UsageRange::Hours24, now_ms)
        .unwrap();
        assert_eq!(empty.coverage.state, CoverageState::Unavailable);
        assert_eq!(empty.totals.total_tokens, None);
    }

    #[test]
    fn repository_probe_uses_the_fixed_json_command_and_private_codex_home() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let executable = temporary.path().join("codex-fixture");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n[ \"$1\" = usage ] || exit 9\n[ \"$2\" = --json ] || exit 9\n[ \"$3\" = repo ] || exit 9\n[ \"$4\" = current ] || exit 9\n[ \"$5\" = --identity-only ] || exit 9\n[ \"$CODEX_HOME\" = \"{}\" ] || exit 9\nprintf '%s\\n' '{{\"schemaVersion\":1,\"kind\":\"usageRepositoryIdentity\",\"databaseSchemaVersion\":4,\"taxonomyVersion\":1,\"scope\":{{\"type\":\"repository\",\"id\":\"{}\"}}}}'\n",
                codex_home.display(),
                "c".repeat(64),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let source = CodexUsageSource {
            uid: rustix::process::getuid().as_raw(),
            codex_home,
            executable,
        };
        let result = HostRepositoryProbe
            .probe(&source, temporary.path(), 1_234)
            .unwrap();
        assert_eq!(result, ("c".repeat(64), 4, 1));
    }

    #[test]
    fn collector_database_must_be_an_owner_file_not_a_symlink() {
        let temporary = tempdir().unwrap();
        let codex_home = temporary.path().join("codex-home");
        std::fs::create_dir_all(codex_home.join("usage")).unwrap();
        let outside = temporary.path().join("outside.sqlite3");
        Connection::open(&outside).unwrap();
        symlink(&outside, codex_home.join("usage/usage.sqlite3")).unwrap();
        let source = CodexUsageSource {
            uid: rustix::process::getuid().as_raw(),
            codex_home,
            executable: temporary.path().join("codex"),
        };
        assert!(matches!(open_source(&source), Err(reason) if reason == "source_unavailable"));
    }

    #[test]
    fn legacy_repository_probe_distinguishes_missing_history_from_failure() {
        let temporary = tempdir().unwrap();
        let executable = temporary.path().join("probe");
        let source = CodexUsageSource {
            uid: rustix::process::getuid().as_raw(),
            codex_home: temporary.path().to_path_buf(),
            executable: executable.clone(),
        };
        for (message, expected) in [
            (
                r#"{"schemaVersion":1,"error":{"code":"not_found"}}"#,
                "mapping_unavailable",
            ),
            (
                r#"{"schemaVersion":1,"error":{"code":"database_unavailable"}}"#,
                "source_unavailable",
            ),
        ] {
            std::fs::write(&executable, format!("#!/bin/sh\nif [ \"$5\" = --identity-only ]; then printf \"unexpected argument '--identity-only'\\n\" >&2; exit 2; fi\n[ \"$3\" = --since ] || exit 9\nprintf '%s\\n' '{message}' >&2\nexit 3\n")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                HostRepositoryProbe.probe(&source, temporary.path(), 1234),
                Err(expected.into())
            );
        }
    }

    #[test]
    fn collection_returns_mapping_pending_while_resolution_runs_off_thread() {
        let temporary = tempdir().unwrap();
        let mut config = config(temporary.path(), temporary.path().join("missing-home"));
        std::fs::create_dir_all(&config.state_dir).unwrap();
        config.codex_usage_sources[0].uid = rustix::process::getuid().as_raw();
        let authority = Database::open(config.database_path()).unwrap();
        let repository = RepositoryRecord {
            repository_id: "r0123456789abcdef".into(),
            display_name: "Example".into(),
            root_path: temporary.path().into(),
        };
        let probe = Arc::new(BlockingProbe {
            entered: AtomicU64::new(0),
            released: AtomicBool::new(false),
        });
        let usage = CodexUsage::with_probe(
            config,
            authority,
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            probe.clone(),
        );
        let started = Instant::now();
        let result = usage
            .repositories(&[repository], UsageRange::Hours24)
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(
            result.repositories[0]
                .coverage
                .snapshot
                .as_ref()
                .unwrap()
                .refreshing
        );
        assert_eq!(result.repositories[0].total_tokens, None);
        probe.released.store(true, Ordering::SeqCst);
        usage.wait_for_refresh(None);
        assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn combining_collectors_adds_tokens_and_unions_wall_time() {
        let source = |phase: &str, tokens: u64, interval: (u64, u64)| SourceReport {
            database_schema: 4,
            taxonomy_version: 1,
            evidence: true,
            tokens: BTreeMap::from([("total_tokens".into(), tokens)]),
            token_observations: BTreeMap::from([("complete".into(), 1)]),
            coverage_events: BTreeMap::from([("complete".into(), 1)]),
            phase_series: vec![BTreeMap::from([(phase.into(), tokens)])],
            bucket_coverage: vec![CoverageState::Complete],
            request_intervals: vec![interval],
            execution_intervals: vec![interval],
            ..SourceReport::default()
        };
        let report = combine(
            &RepositoryRecord {
                repository_id: "r1".into(),
                display_name: "Example".into(),
                root_path: "/repo".into(),
            },
            UsageRange::Hours24,
            15,
            0,
            100,
            1,
            &[
                (1, source("implementation", 100, (0, 10))),
                (2, source("testing", 50, (5, 15))),
            ],
            BTreeMap::new(),
            2,
        );
        assert_eq!(report.coverage.state, CoverageState::Complete);
        assert_eq!(report.totals.total_tokens, Some(150));
        assert_eq!(report.series[0].phases["implementation"], 100);
        assert_eq!(report.series[0].phases["testing"], 50);
        assert_eq!(report.time.execution_wall.measured_ms, 15);
    }
}
