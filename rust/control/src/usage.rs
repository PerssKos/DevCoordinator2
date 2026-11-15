//! Privacy-preserving read-only aggregation of configured Codex usage collectors.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::params::{
    UsageRange, UsageRepositories as UsageRepositoriesParams,
    UsageRepository as UsageRepositoryParams,
};
use devcoordinator2_api::results::{
    CoverageState, MeasuredTime, PhaseTime, ToolFamily, ToolOutcome, UsageActivity, UsageCost,
    UsageCoverage, UsageModelCost, UsageOutcome, UsageRepositories, UsageRepository,
    UsageRepositoryRow, UsageSemantics, UsageSeriesPoint, UsageSnapshot, UsageTime, UsageTools,
    UsageTotals, UsageWorktree, UsageWorktreeScope,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
#[cfg(test)]
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use time::{format_description::FormatItem, macros::format_description};

use crate::config::{CodexUsageSource, Config};
use crate::database::{Database, DatabaseError, OptionalExtension as _};
use crate::platform::{Clock, HostClock};
use crate::rate_card::RateCardSnapshot;
use crate::repository::Registry;

#[path = "usage_cost.rs"]
mod cost;
use cost::{CostBuckets, cost_from_buckets, merge_cost_bucket};
#[cfg(test)]
use cost::{RequestTokens, aggregate_cost};

#[path = "usage_cache.rs"]
mod cache;
#[path = "usage_api.rs"]
mod collector_api;
#[cfg(test)]
#[path = "usage_performance.rs"]
mod performance;
#[path = "usage_review.rs"]
mod review;
#[path = "usage_review_aggregate.rs"]
mod review_aggregate;
#[cfg(test)]
#[path = "usage_review_facts.rs"]
mod review_facts;
#[path = "usage_review_math.rs"]
mod review_math;
#[cfg(test)]
#[path = "usage_review_query.rs"]
mod review_query;
#[cfg(test)]
#[path = "usage_review_tests.rs"]
mod review_tests;

const SUPPORTED_TAXONOMY: u32 = 1;

#[derive(Clone, Copy, Debug)]
enum Projection {
    Full,
    Tokens,
    PerformanceFast,
}
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_POLL: Duration = Duration::from_millis(10);
#[cfg(test)]
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
    (
        "input_tokens_details.cache_write_tokens",
        TokenField::CacheWrite,
    ),
];
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// A source is fresh when its v2 manifest is present. The existence check is
/// deliberately metadata-only; it never opens the retired SQLite database.
fn has_usage_v2_manifest(source: &CodexUsageSource) -> bool {
    source
        .codex_home
        .join("usage-v2/manifest.json")
        .metadata()
        .is_ok_and(|metadata| metadata.is_file())
}

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
    cache: cache::UsageCache,
    api: collector_api::CollectorApi,
    binding_failures: Arc<Mutex<BTreeSet<(u32, String)>>>,
}

#[derive(Clone, Debug)]
pub struct RepositoryRecord {
    pub repository_id: String,
    pub display_name: String,
    pub root_path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct WorktreeSelection {
    scope: UsageWorktreeScope,
    include_unassigned: bool,
    source_keys: BTreeMap<u32, Option<Vec<String>>>,
    unavailable_sources: BTreeSet<u32>,
}

#[derive(Clone, Copy, Debug)]
enum TokenField {
    Total,
    Input,
    CachedInput,
    Output,
    Reasoning,
    CacheWrite,
}

#[derive(Default)]
struct SourceReport {
    source_snapshot: Option<devcoordinator2_api::results::UsageSourceSnapshot>,
    supplied_cost: Option<UsageCost>,
    snapshot: Option<devcoordinator2_api::results::UsageSnapshot>,
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
    activity_costs: BTreeMap<(String, String), CostBuckets>,
    outcome_costs: BTreeMap<String, CostBuckets>,
}

#[derive(Default)]
#[cfg(test)]
struct CachedCostAggregate {
    values: BTreeMap<String, u64>,
    observations: u64,
    unknown_observations: u64,
    latest_at_ms: u64,
}

#[derive(Clone)]
#[cfg(test)]
struct Operation {
    id: String,
    kind: String,
    #[allow(dead_code)] // Historical SQL conformance fixture metadata.
    agent_id: Option<String>,
    started_at_ms: u64,
    #[allow(dead_code)] // Historical SQL conformance fixture metadata.
    finished_at_ms: Option<u64>,
    phase: String,
    activity: String,
    #[allow(dead_code)] // Historical SQL conformance fixture metadata.
    activity_state: String,
    provenance: String,
    terminal_event: Option<String>,
    tool_family: Option<String>,
    provider_kind: Option<String>,
    model: Option<String>,
    #[allow(dead_code)] // Historical SQL conformance fixture metadata.
    outcome_id: Option<String>,
}

impl UsageService {
    pub(crate) fn binding_failed(&self, uid: u32, repository: String) {
        if let Ok(mut failures) = self.usage.binding_failures.lock() {
            failures.insert((uid, repository));
        }
    }

    #[cfg(test)]
    pub(crate) fn binding_capture_failed(&self, uid: u32, repository: &str) -> bool {
        self.usage.binding_capture_failed(uid, repository)
    }

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
            usage: CodexUsage::with_clock(config, database, clock),
        }
    }

    pub fn usage(&self) -> &CodexUsage {
        &self.usage
    }

    #[cfg(test)]
    pub(crate) fn expire_collector_cache(&self) {
        self.usage.api.expire();
    }

    #[cfg(test)]
    pub(crate) fn wait_for_collector_refresh(&self, deadline: Instant) -> bool {
        self.usage.api.wait_until_idle(deadline)
    }

    pub fn repositories(
        &self,
        params: UsageRepositoriesParams,
    ) -> Result<UsageRepositories, ProtocolError> {
        let records = self.records()?;
        let report = self.usage.repositories(&records, params.range.clone())?;
        if params.wait_for_refresh {
            if report.repositories.iter().all(|row| {
                row.coverage.available_collectors > 0
                    && !row
                        .coverage
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.refreshing)
            }) {
                return Ok(report);
            }
            self.usage.wait_for_refresh(None);
            return self.usage.repositories_tokens(&records, params.range);
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
        let selection = self.usage.resolve_worktree_selection(
            &repository,
            params.worktree_ids.as_deref(),
            params.include_unassigned,
            self.usage.now_ms()?,
            None,
        )?;
        // Paint the indexed token projection first. Large collectors can take
        // much longer to assemble activity, outcome, tool, and timing joins;
        // starting that full query on the first request would block the cache
        // worker and leave the user with an all-empty detail page. The client
        // follows the refreshing snapshot with wait_for_refresh=true, which
        // then requests the full projection.
        let mut report = if params.wait_for_refresh {
            self.usage.repository_full_with_selection(
                &repository,
                params.range.clone(),
                selection.clone(),
            )?
        } else {
            self.usage.repository_projection(
                &repository,
                params.range.clone(),
                Projection::Tokens,
                Some(selection.clone()),
            )?
        };
        if !params.wait_for_refresh
            && report.coverage.available_collectors > 0
            && let Some(snapshot) = report.coverage.snapshot.as_mut()
        {
            snapshot.refreshing = true;
        }
        if !params.wait_for_refresh
            && params.worktree_ids.is_none()
            && params.include_unassigned
            && self
                .usage
                .config
                .codex_usage_sources
                .iter()
                .any(|s| s.api_socket.is_some())
            && report
                .coverage
                .snapshot
                .as_ref()
                .is_some_and(|s| s.updated_at_ms.is_none() && s.refreshing)
        {
            self.usage.wait_for_refresh(Some(&repository.repository_id));
            report = self.usage.repository_full_with_selection(
                &repository,
                params.range.clone(),
                selection.clone(),
            )?;
        }
        self.attach_outcome_titles(&mut report)?;
        if params.wait_for_refresh {
            self.usage.wait_for_refresh(Some(&repository.repository_id));
            let mut report = self.usage.repository_full_with_selection(
                &repository,
                params.range.clone(),
                selection.clone(),
            )?;
            if report.coverage.available_collectors == 0 && report.totals.total_tokens.is_none() {
                let fast = self.usage.repository_projection(
                    &repository,
                    params.range,
                    Projection::Tokens,
                    Some(selection),
                )?;
                if fast.coverage.available_collectors > 0 || fast.totals.total_tokens.is_some() {
                    report = fast;
                    report.coverage.has_gaps = true;
                    increment(
                        &mut report.coverage.unavailable_reasons,
                        "detail_unavailable",
                    );
                    report.coverage.state = CoverageState::Partial;
                }
            }
            self.attach_outcome_titles(&mut report)?;
            return Ok(report);
        }
        Ok(report)
    }

    fn attach_outcome_titles(&self, report: &mut UsageRepository) -> Result<(), ProtocolError> {
        if report.outcomes.is_empty() {
            return Ok(());
        }
        let ids = serde_json::to_string(
            &report
                .outcomes
                .iter()
                .map(|outcome| outcome.outcome_id.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| ProtocolError::new(ErrorCode::InternalError, "cannot encode outcome ids"))?;
        let repository_id = report.repository_id.clone();
        let titles = self
            .usage
            .authority
            .read(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT task_id,title FROM tasks WHERE repository_id=$1 AND task_id IN (SELECT jsonb_array_elements_text($2::jsonb))",
                )?;
                let rows = statement
                    .query_map(crate::database::params![repository_id, ids], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<BTreeMap<_, _>, _>>()?;
                Ok(rows)
            })
            .map_err(database_error)?;
        for outcome in &mut report.outcomes {
            outcome.title = titles.get(&outcome.outcome_id).cloned();
        }
        Ok(())
    }

    pub(crate) fn review_window(
        &self,
        repository: &RepositoryRecord,
        workstream: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        deadline: Instant,
    ) -> Result<devcoordinator2_api::review::ReviewUsage, ProtocolError> {
        self.usage
            .review_measurements(repository, workstream, start_ms, end_ms, deadline)
    }

    pub(crate) fn performance_window(
        &self,
        repository: &RepositoryRecord,
        workstream: Option<&str>,
        start: u64,
        end: u64,
        deadline: Instant,
    ) -> Result<devcoordinator2_api::review::ReviewUsage, ProtocolError> {
        self.usage.measurements(
            repository,
            workstream,
            start,
            end,
            deadline,
            review::Projection::Tokens,
        )
    }

    pub(crate) fn performance_tokens(
        &self,
        repository: &RepositoryRecord,
        start: u64,
        end: u64,
        now: u64,
    ) -> Result<UsageRepository, ProtocolError> {
        let rate_cards = self.usage.rate_cards()?;
        self.usage.repository_window(
            repository,
            UsageRange::Hours24,
            now,
            start,
            end,
            end - start,
            1,
            true,
            Some(Instant::now() + QUERY_TIMEOUT),
            Projection::Tokens,
            rate_cards,
            None,
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
    pub(crate) fn binding_capture_failed(&self, uid: u32, repository: &str) -> bool {
        self.binding_failures.lock().map_or(true, |failures| {
            failures.contains(&(uid, repository.to_owned()))
        })
    }

    pub fn new(config: Config, authority: Database) -> Self {
        Self::with_clock(config, authority, Arc::new(HostClock))
    }
    pub fn with_clock(config: Config, authority: Database, clock: Arc<dyn Clock>) -> Self {
        Self {
            config: Arc::new(config),
            authority,
            clock,
            cache: cache::UsageCache::default(),
            api: collector_api::CollectorApi::default(),
            binding_failures: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }
    pub fn repositories(
        &self,
        repositories: &[RepositoryRecord],
        range: UsageRange,
    ) -> Result<UsageRepositories, ProtocolError> {
        if let Some(report) = self.api_repositories(repositories, range.clone())? {
            return Ok(report);
        }
        let now_ms = self.now_ms()?;
        let mut rows = Vec::new();
        for repository in repositories {
            let report = self.repository_fast(repository, range.clone())?;
            rows.push(UsageRepositoryRow {
                repository_id: report.repository_id,
                display_name: report.display_name,
                range: report.range,
                coverage: report.coverage,
                total_tokens: report.totals.total_tokens,
                model_requests: Some(report.totals.model_requests),
                tool_calls: Some(report.totals.tool_calls),
                execution_wall_ms: Some(report.time.execution_wall.measured_ms),
                cost: report.totals.cost.clone(),
            });
        }
        Ok(UsageRepositories {
            range,
            generated_at_ms: now_ms,
            repositories: rows,
        })
    }

    pub fn repositories_tokens(
        &self,
        repositories: &[RepositoryRecord],
        range: UsageRange,
    ) -> Result<UsageRepositories, ProtocolError> {
        let now_ms = self.now_ms()?;
        let mut rows = Vec::new();
        for repository in repositories {
            // Consume the same display entry started by repositories().
            // Switching projections here starts a second, empty cache entry
            // after the first entry has already finished refreshing.
            let report = self.repository_fast(repository, range.clone())?;
            rows.push(UsageRepositoryRow {
                repository_id: report.repository_id,
                display_name: report.display_name,
                range: report.range,
                coverage: report.coverage,
                total_tokens: report.totals.total_tokens,
                model_requests: Some(report.totals.model_requests),
                tool_calls: Some(report.totals.tool_calls),
                execution_wall_ms: Some(report.time.execution_wall.measured_ms),
                cost: report.totals.cost.clone(),
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
        // Detail pages promise token components and API-equivalent cost. The
        // bounded cache keeps this indexed projection off the request path.
        self.repository_projection(repository, range, Projection::Tokens, None)
    }

    pub fn repository_fast(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
    ) -> Result<UsageRepository, ProtocolError> {
        self.repository_projection(repository, range, Projection::PerformanceFast, None)
    }

    pub fn repository_tokens(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
    ) -> Result<UsageRepository, ProtocolError> {
        self.repository_projection(repository, range, Projection::Tokens, None)
    }

    pub(crate) fn repository_full_with_selection(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        selection: WorktreeSelection,
    ) -> Result<UsageRepository, ProtocolError> {
        self.repository_projection(repository, range, Projection::Full, Some(selection))
    }

    fn repository_projection(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        projection: Projection,
        selection: Option<WorktreeSelection>,
    ) -> Result<UsageRepository, ProtocolError> {
        let now_ms = self.now_ms()?;
        let rate_cards = self.rate_cards()?;
        let (start, end, bucket, count) = usage_window(&range, now_ms);
        self.cached_window(
            repository, range, now_ms, start, end, bucket, count, true, projection, rate_cards,
            selection,
        )
    }

    pub fn repository_at(
        &self,
        repository: &RepositoryRecord,
        range: UsageRange,
        now_ms: u64,
    ) -> Result<UsageRepository, ProtocolError> {
        let (start, end, bucket, count) = usage_window(&range, now_ms);
        let rate_cards = self.rate_cards()?;
        self.repository_window(
            repository,
            range,
            now_ms,
            start,
            end,
            bucket,
            count,
            true,
            None,
            Projection::Full,
            rate_cards,
            None,
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
        let rate_cards = self.rate_cards()?;
        self.cached_window(
            repository,
            range,
            now_ms,
            start,
            now_ms,
            bucket_ms,
            bucket_count,
            resolve_missing,
            Projection::Tokens,
            rate_cards,
            None,
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
        projection: Projection,
        rate_cards: RateCardSnapshot,
        selection: Option<WorktreeSelection>,
    ) -> Result<UsageRepository, ProtocolError> {
        // A single-bucket Performance window has exact end semantics. Live
        // chart snapshots are reusable within their fixed UTC bucket window.
        let end_key = if bucket_count == 1 { end_ms } else { 0 };
        let key = format!(
            "{}:{:?}:{}:{start_ms}:{end_key}:{bucket_ms}:{bucket_count}:{projection:?}:scope-{:?}:unassigned-{}:rate-{}",
            repository.repository_id,
            repository.root_path,
            range_name(&range),
            selection
                .as_ref()
                .and_then(|selection| selection.scope.selected_worktree_ids.as_ref()),
            selection
                .as_ref()
                .is_none_or(|selection| selection.include_unassigned),
            rate_cards.revision
        );
        let mut empty = combine(
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
        if let Some(selection) = &selection {
            empty.worktree_scope = Some(selection.scope.clone());
        }
        let progress = self
            .config
            .codex_usage_sources
            .iter()
            .filter_map(source_progress)
            .collect::<Vec<_>>();
        if !progress.is_empty() {
            let completed = progress
                .iter()
                .filter_map(|snapshot| snapshot.progress_completed)
                .sum::<u64>();
            let total = progress
                .iter()
                .filter_map(|snapshot| snapshot.progress_total)
                .sum::<u64>();
            empty.coverage.snapshot = Some(UsageSnapshot {
                updated_at_ms: None,
                refreshing: completed < total,
                refresh_failed: false,
                progress_completed: Some(completed),
                progress_total: Some(total),
                progress_stage: progress
                    .iter()
                    .find_map(|snapshot| snapshot.progress_stage.clone()),
            });
        }
        let usage = self.clone();
        let repository = repository.clone();
        Ok(self.cache.get_window(key, end_ms, empty, move |cutoff_ms| {
            // Identity mapping belongs to this existing background loader,
            // never the Console request. One shared budget bounds every path.
            let selection = selection
                .map(|selection| {
                    usage.resolve_worktree_selection(
                        &repository,
                        selection.scope.selected_worktree_ids.as_deref(),
                        selection.include_unassigned,
                        now_ms,
                        Some(Instant::now() + Duration::from_secs(5)),
                    )
                })
                .transpose()?;
            usage.repository_window(
                &repository,
                range,
                now_ms,
                start_ms,
                cutoff_ms,
                bucket_ms,
                bucket_count,
                resolve_missing,
                None,
                projection,
                rate_cards,
                selection,
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
        projection: Projection,
        rate_cards: RateCardSnapshot,
        selection: Option<WorktreeSelection>,
    ) -> Result<UsageRepository, ProtocolError> {
        let mut reports = Vec::new();
        let mut failures = BTreeMap::new();
        let mut progress = Vec::new();
        let deadline = Some(deadline.unwrap_or_else(|| Instant::now() + QUERY_TIMEOUT));
        for source in &self.config.codex_usage_sources {
            let source_snapshot = source_progress(source);
            let source_indexing = source_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.refreshing);
            if let Some(snapshot) = source_snapshot {
                progress.push(snapshot);
            }
            // A cold producer owns the canonical SQLite backfill. Reading its
            // raw tables here can hold a WAL snapshot for minutes (or longer),
            // which prevents the producer from committing the next derived
            // page. Keep the fast response truthful and let the producer finish
            // its indexed cache before scheduling a fallback scan.
            if source_indexing {
                increment(&mut failures, "indexing");
                continue;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                increment(&mut failures, "query_budget_exhausted");
                continue;
            }
            if let Some(selection) = &selection
                && selection.unavailable_sources.contains(&source.uid)
            {
                increment(&mut failures, "worktree_mapping_unavailable");
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
                        projection,
                        &rate_cards.cards,
                        selection
                            .as_ref()
                            .and_then(|selection| selection.source_keys.get(&source.uid)),
                        selection
                            .as_ref()
                            .map(|selection| selection.include_unassigned),
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
                                        projection,
                                        &rate_cards.cards,
                                        selection.as_ref().and_then(|selection| {
                                            selection.source_keys.get(&source.uid)
                                        }),
                                        selection
                                            .as_ref()
                                            .map(|selection| selection.include_unassigned),
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
        let mut report = combine(
            repository,
            range,
            now_ms,
            start_ms,
            bucket_ms,
            bucket_count,
            &reports,
            failures,
            self.config.codex_usage_sources.len(),
        );
        if let Some(mut selection) = selection {
            selection.scope.attribution_available = reports
                .iter()
                .any(|(_, source)| source.database_schema >= 9);
            report.worktree_scope = Some(selection.scope);
        }
        if !progress.is_empty() {
            let completed = progress
                .iter()
                .filter_map(|snapshot| snapshot.progress_completed)
                .sum::<u64>();
            let total = progress
                .iter()
                .filter_map(|snapshot| snapshot.progress_total)
                .sum::<u64>();
            let stage = progress
                .iter()
                .find_map(|snapshot| snapshot.progress_stage.clone());
            let mut snapshot = report.coverage.snapshot.take().unwrap_or(UsageSnapshot {
                updated_at_ms: None,
                refreshing: false,
                refresh_failed: false,
                progress_completed: None,
                progress_total: None,
                progress_stage: None,
            });
            snapshot.progress_completed = Some(completed);
            snapshot.progress_total = Some(total);
            snapshot.progress_stage = stage;
            snapshot.refreshing |= completed < total;
            report.coverage.snapshot = Some(snapshot);
        }
        Ok(report)
    }

    fn repository_key(
        &self,
        source: &CodexUsageSource,
        repository: &RepositoryRecord,
        now_ms: u64,
        resolve_missing: bool,
    ) -> Result<String, ProtocolError> {
        self.repository_key_until(
            source,
            repository,
            now_ms,
            resolve_missing,
            Instant::now() + collector_api::API_BUDGET,
        )
    }

    fn repository_key_until(
        &self,
        source: &CodexUsageSource,
        repository: &RepositoryRecord,
        now_ms: u64,
        resolve_missing: bool,
        deadline: Instant,
    ) -> Result<String, ProtocolError> {
        if !has_usage_v2_manifest(source) {
            return Err(source_error("legacy_source_unsupported"));
        }
        let uid = source.uid;
        let repository_id = repository.repository_id.clone();
        let cached = self.authority.read(move |connection| {
            connection.query_row("SELECT codex_repository_id FROM codex_usage_repository_links WHERE source_uid=$1 AND repository_id=$2 AND source_schema=3 AND taxonomy_version=1", crate::database::params![uid,repository_id], |row| row.get::<_,String>(0)).optional().map_err(DatabaseError::from)
        }).map_err(database_error)?.filter(|key| valid_repository_key(key));
        if let Some(key) = cached {
            return Ok(key);
        }
        if !resolve_missing {
            return Err(source_error("mapping_pending"));
        }
        let key = collector_api::repository_identity(
            source,
            &repository.root_path,
            deadline.min(Instant::now() + collector_api::API_BUDGET),
        )
        .map_err(source_error)?;
        let resolved_at = self.timestamp()?;
        let repository_id = repository.repository_id.clone();
        self.authority.transaction(|tx| {
            tx.execute("INSERT INTO codex_usage_repository_links(source_uid,repository_id,codex_repository_id,source_schema,taxonomy_version,resolved_at) VALUES($1,$2,$3,3,1,$4) ON CONFLICT(source_uid,repository_id) DO UPDATE SET codex_repository_id=EXCLUDED.codex_repository_id,source_schema=3,taxonomy_version=1,resolved_at=EXCLUDED.resolved_at", crate::database::params![uid,repository_id,key,resolved_at])?;
            Ok(())
        }).map_err(database_error)?;
        let _ = now_ms;
        Ok(key)
    }

    fn delete_link(&self, uid: u32, repository_id: &str) -> Result<(), ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.authority
            .transaction(move |transaction| {
                transaction.execute(
                    "DELETE FROM codex_usage_repository_links WHERE source_uid=$1 AND repository_id=$2",
                    crate::database::params![uid,repository_id],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    fn resolve_worktree_selection(
        &self,
        repository: &RepositoryRecord,
        requested: Option<&[String]>,
        include_unassigned: bool,
        now_ms: u64,
        deadline: Option<Instant>,
    ) -> Result<WorktreeSelection, ProtocolError> {
        let repository_id = repository.repository_id.clone();
        let rows = self
            .authority
            .read(move |connection| {
                let mut query = connection.prepare(
                    "SELECT worktree_id,worktree_path FROM worktrees WHERE repository_id=$1 ORDER BY rowid",
                )?;
                query
                    .query_map([repository_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        let mut rows = rows
            .into_iter()
            .map(|(id, path)| (id, PathBuf::from(path)))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            let left_root = left.1 == repository.root_path;
            let right_root = right.1 == repository.root_path;
            right_root
                .cmp(&left_root)
                .then_with(|| left.0.cmp(&right.0))
        });

        let requested_set = requested.map(|ids| ids.iter().cloned().collect::<BTreeSet<_>>());
        if let Some(ids) = &requested_set
            && ids
                .iter()
                .any(|id| !rows.iter().any(|(known, _)| known == id))
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "usage worktree is not registered for this repository",
            ));
        }

        let mut source_keys_by_worktree = BTreeMap::<String, BTreeMap<u32, Option<String>>>::new();
        let paths_to_probe = requested_set.as_ref().map(|ids| {
            rows.iter()
                .filter(|(id, _)| ids.contains(id))
                .collect::<Vec<_>>()
        });
        for (worktree_id, path) in paths_to_probe.into_iter().flatten() {
            for source in &self.config.codex_usage_sources {
                let key = self.cached_worktree_key(source, path, now_ms, deadline)?;
                source_keys_by_worktree
                    .entry(worktree_id.clone())
                    .or_default()
                    .insert(source.uid, key);
            }
        }
        let mut labels = rows
            .iter()
            .map(|(id, path)| {
                let basename = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("repository")
                    .to_owned();
                (id.clone(), basename)
            })
            .collect::<BTreeMap<_, _>>();
        if labels.values().collect::<BTreeSet<_>>().len() != labels.len() {
            for (id, path) in &rows {
                let base = labels
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "repository".into());
                let parent = path
                    .parent()
                    .and_then(|value| value.file_name())
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("root");
                labels.insert(id.clone(), format!("{parent}/{base}"));
            }
        }
        let mut scope_rows = Vec::with_capacity(rows.len());
        for (id, _) in &rows {
            let available = requested.is_some()
                && self.config.codex_usage_sources.iter().any(|source| {
                    source_keys_by_worktree
                        .get(id)
                        .and_then(|keys| keys.get(&source.uid))
                        .and_then(Option::as_ref)
                        .is_some()
                });
            scope_rows.push(UsageWorktree {
                worktree_id: id.clone(),
                label: labels
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "repository".into()),
                available,
            });
        }

        let selected_paths = requested.map(|ids| {
            rows.iter()
                .filter(|(id, _)| ids.iter().any(|selected| selected == id))
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>()
        });
        let mut source_keys = BTreeMap::new();
        let mut unavailable_sources = BTreeSet::new();
        for source in &self.config.codex_usage_sources {
            let keys = selected_paths.as_ref().map(|paths| {
                let mut keys = Vec::with_capacity(paths.len());
                for path in paths {
                    let id = rows
                        .iter()
                        .find(|(_, candidate)| candidate == path)
                        .map(|(id, _)| id)
                        .expect("selected worktree path came from registered rows");
                    match source_keys_by_worktree
                        .get(id)
                        .and_then(|values| values.get(&source.uid))
                        .and_then(Option::clone)
                    {
                        Some(key) => keys.push(key),
                        None => {
                            unavailable_sources.insert(source.uid);
                        }
                    }
                }
                keys
            });
            source_keys.insert(source.uid, keys);
        }
        Ok(WorktreeSelection {
            scope: UsageWorktreeScope {
                worktrees: scope_rows,
                selected_worktree_ids: requested.map(|ids| ids.to_vec()),
                include_unassigned,
                attribution_available: requested.is_none()
                    || self
                        .config
                        .codex_usage_sources
                        .iter()
                        .any(|source| !unavailable_sources.contains(&source.uid)),
            },
            include_unassigned,
            source_keys,
            unavailable_sources,
        })
    }

    fn cached_worktree_key(
        &self,
        source: &CodexUsageSource,
        path: &Path,
        now_ms: u64,
        deadline: Option<Instant>,
    ) -> Result<Option<String>, ProtocolError> {
        // The v1 API does not resolve worktree paths. Only explicit producer
        // worktree keys are accepted; the retired CLI is never executed.
        let _ = (source, path, now_ms, deadline);
        Ok(None)
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
        projection: Projection,
        rate_cards: &[devcoordinator2_api::rate_card::RateCard],
        worktree_keys: Option<&Option<Vec<String>>>,
        include_unassigned: Option<bool>,
    ) -> Result<SourceReport, String> {
        let selected_keys = worktree_keys.and_then(Option::as_deref);
        let include_unassigned = include_unassigned.unwrap_or(true);
        let filtered = selected_keys.is_some() || !include_unassigned;
        let deadline = deadline.unwrap_or_else(|| Instant::now() + QUERY_TIMEOUT);
        let _ = (bucket_ms, projection, rate_cards);
        if Instant::now() >= deadline {
            return Err("query_budget_exhausted".into());
        }
        if has_usage_v2_manifest(source) {
            let summary = self
                .api
                .summary(
                    source,
                    Some(repository_key),
                    worktree_keys.and_then(Option::as_deref),
                    include_unassigned,
                    start_ms,
                    end_ms,
                    deadline.min(Instant::now() + collector_api::API_BUDGET),
                )
                .map_err(|reason| {
                    // A fresh store is available only through a successful,
                    // versioned handshake. Never fall back to legacy SQLite.
                    if filtered && reason == "api_contract_unsupported" {
                        "worktree_attribution_unavailable".to_owned()
                    } else {
                        reason
                    }
                })?;
            if filtered
                && (summary.report.worktree_coverage.is_none()
                    || summary.source.as_ref().is_some_and(|source| {
                        source
                            .get("coverageState")
                            .or_else(|| source.get("coverage_state"))
                            .and_then(serde_json::Value::as_str)
                            == Some("unavailable")
                    }))
            {
                return Err("worktree_attribution_unavailable".into());
            }
            return Ok(summary.source_report(
                None,
                bucket_count,
                worktree_keys.and_then(Option::as_deref),
                include_unassigned,
            ));
        }
        Err("legacy_source_unsupported".into())
    }

    fn rate_cards(&self) -> Result<RateCardSnapshot, ProtocolError> {
        self.authority
            .read(|connection| Ok(crate::rate_card::snapshot_rows(connection)?))
            .map_err(database_error)
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

fn source_progress(_: &CodexUsageSource) -> Option<UsageSnapshot> {
    None
}

#[cfg(test)]
fn has_worktree_attribution(connection: &Connection) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('repository_attributions') WHERE name='worktree_id')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "source_unavailable".into())
}

/// Retain facts by their effective token-owning operation. A covered tool
/// observation therefore follows its request rather than its reporting tool.
/// The same facts feed totals, components, activity, model cost and the chart.
#[cfg(test)]
fn filter_worktree_facts(
    connection: &Connection,
    family: &[String],
    facts: &mut review_facts::Facts,
    selected: Option<&[String]>,
    include_unassigned: bool,
) -> Result<(), String> {
    let family = serde_json::to_string(family).map_err(|_| "source_unavailable")?;
    let ids = serde_json::to_string(&facts.operations.keys().collect::<Vec<_>>())
        .map_err(|_| "source_unavailable")?;
    let mut query = connection
        .prepare("SELECT operation_id,worktree_id FROM repository_attributions WHERE repository_id IN (SELECT value FROM json_each(?1)) AND operation_id IN (SELECT value FROM json_each(?2))")
        .map_err(|_| "source_unavailable")?;
    let rows = query
        .query_map(rusqlite::params![family, ids], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|_| "source_unavailable")?;
    let mut assigned = BTreeSet::new();
    let mut matching = BTreeSet::new();
    for row in rows {
        let (owner, key) = row.map_err(|_| "source_unavailable")?;
        if let Some(key) = key {
            assigned.insert(owner.clone());
            if selected.is_none_or(|selected| selected.contains(&key)) {
                matching.insert(owner);
            }
        }
    }
    facts
        .operations
        .retain(|id, _| matching.contains(id) || include_unassigned && !assigned.contains(id));
    facts
        .tokens
        .retain(|token| facts.operations.contains_key(&token.owner));
    facts
        .waits
        .retain(|owner, _| facts.operations.contains_key(owner));
    Ok(())
}

#[cfg(test)]
fn add_cached_dimension_costs(
    connection: &Connection,
    report: &mut SourceReport,
    family: &[String],
    start_ms: u64,
    end_ms: u64,
    rate_cards: &[devcoordinator2_api::rate_card::RateCard],
) -> Result<(), String> {
    let repositories = serde_json::to_string(family).map_err(|_| "source_unavailable")?;
    let lower_hour = i64_value(start_ms / 3_600_000)?;
    let upper_hour = i64_value(end_ms.saturating_add(3_599_999) / 3_600_000)?;
    let sql = r#"
        SELECT hour_index, model, provider_kind, phase, activity, provenance,
               category_path, measured_tokens, unknown_observations,
               observation_count, coverage_state
          FROM _usage_report_dimension_tokens INDEXED BY sqlite_autoindex__usage_report_dimension_tokens_1
         WHERE hour_index >= ?2 AND hour_index < ?3
           AND repository_bucket IN (SELECT value FROM json_each(?1))
           AND measurement_provenance = 'provider_reported'
           AND category_path IN (
             'input_tokens', 'input_tokens_details.cached_tokens',
             'input_tokens_details.cache_write_tokens', 'output_tokens',
             'output_tokens_details.reasoning_tokens', 'total_tokens'
           )
    "#;
    let mut statement = connection.prepare(sql).map_err(|_| "source_unavailable")?;
    let mut rows = statement
        .query(rusqlite::params![repositories, lower_hour, upper_hour])
        .map_err(|_| "source_unavailable")?;
    let mut groups =
        BTreeMap::<(String, String, String, String, String), CachedCostAggregate>::new();
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let hour: u64 = row
            .get::<_, i64>(0)
            .map_err(|_| "source_unavailable")?
            .try_into()
            .map_err(|_| "source_unavailable")?;
        let model = safe_label(&row.get::<_, String>(1).map_err(|_| "source_unavailable")?);
        let provider = safe_label(&row.get::<_, String>(2).map_err(|_| "source_unavailable")?);
        let phase = safe_phase(&row.get::<_, String>(3).map_err(|_| "source_unavailable")?);
        let activity = safe_label(&row.get::<_, String>(4).map_err(|_| "source_unavailable")?);
        let provenance = safe_label(&row.get::<_, String>(5).map_err(|_| "source_unavailable")?);
        let category = row.get::<_, String>(6).map_err(|_| "source_unavailable")?;
        let value: u64 = row
            .get::<_, i64>(7)
            .map_err(|_| "source_unavailable")?
            .try_into()
            .map_err(|_| "source_unavailable")?;
        let unknown: u64 = row
            .get::<_, i64>(8)
            .map_err(|_| "source_unavailable")?
            .try_into()
            .map_err(|_| "source_unavailable")?;
        let observations: u64 = row
            .get::<_, i64>(9)
            .map_err(|_| "source_unavailable")?
            .try_into()
            .map_err(|_| "source_unavailable")?;
        let coverage = safe_coverage(&row.get::<_, String>(10).map_err(|_| "source_unavailable")?);
        let key = (phase, activity, provenance, model, provider);
        let group = groups.entry(key).or_default();
        *group.values.entry(category).or_default() = group
            .values
            .get(&category)
            .copied()
            .unwrap_or(0)
            .saturating_add(value);
        group.observations = group.observations.max(observations);
        group.unknown_observations = group
            .unknown_observations
            .saturating_add(unknown)
            .saturating_add(u64::from(coverage != CoverageState::Complete));
        group.latest_at_ms = group.latest_at_ms.max(hour.saturating_mul(3_600_000));
    }
    for ((phase, activity, _provenance, model, provider), group) in groups {
        let Some(observations) = (group.observations > 0).then_some(group.observations) else {
            continue;
        };
        report.evidence = true;
        for (category, value) in &group.values {
            if category == "total_tokens" && report.tokens.contains_key(category) {
                continue;
            }
            *report.tokens.entry(category.clone()).or_default() = report
                .tokens
                .get(category)
                .copied()
                .unwrap_or(0)
                .saturating_add(*value);
        }
        let bucket = aggregate_cost(
            &provider,
            &model,
            &group.values,
            observations,
            group.unknown_observations > 0,
            group.latest_at_ms,
            rate_cards,
        );
        merge_cost_bucket(
            report
                .activity_costs
                .entry((phase.clone(), activity.clone()))
                .or_default(),
            &bucket,
        );
        report.model_request_count = report.model_request_count.saturating_add(observations);
        report.operation_count = report.operation_count.saturating_add(observations);
        *report
            .activity_operations
            .entry((phase, activity))
            .or_default() += observations;
    }
    Ok(())
}

#[cfg(test)]
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
    let mut activity_costs = BTreeMap::<(String, String), CostBuckets>::new();
    let mut outcome_costs = BTreeMap::<String, CostBuckets>::new();
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
        for (key, value) in &report.activity_costs {
            merge_cost_bucket(activity_costs.entry(key.clone()).or_default(), value);
        }
        for (key, value) in &report.outcome_costs {
            merge_cost_bucket(outcome_costs.entry(key.clone()).or_default(), value);
        }
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
    let mut total_cost_buckets = CostBuckets::default();
    for bucket in activity_costs.values() {
        merge_cost_bucket(&mut total_cost_buckets, bucket);
    }
    total_cost_buckets.source_gaps += failures.values().sum::<u64>();
    let total_cost = if reports
        .iter()
        .any(|(_, report)| report.supplied_cost.is_some())
    {
        let estimates = reports
            .iter()
            .map(|(_, report)| {
                report.supplied_cost.clone().unwrap_or_else(|| {
                    let mut buckets = CostBuckets::default();
                    for value in report.activity_costs.values() {
                        merge_cost_bucket(&mut buckets, value);
                    }
                    cost_from_buckets(&buckets)
                })
            })
            .collect::<Vec<_>>();
        collector_api::merge_costs(&estimates, !failures.is_empty())
    } else {
        cost_from_buckets(&total_cost_buckets)
    };
    let mut model_rows = total_cost_buckets
        .models
        .iter()
        .map(|(model, parts)| {
            let cost = cost_from_buckets(&CostBuckets {
                models: BTreeMap::from([(model.clone(), parts.clone())]),
                tokens: parts.tokens,
                operations: parts.requests,
                ..Default::default()
            });
            UsageModelCost {
                model: model
                    .split_once('/')
                    .map_or(model.as_str(), |(_, name)| name)
                    .to_owned(),
                total_tokens: parts.tokens,
                share: total_tokens
                    .filter(|total| *total > 0)
                    .map(|total| parts.tokens as f64 / total as f64),
                model_requests: parts.requests,
                average_usd_micros: cost
                    .estimated_usd_micros
                    .and_then(|value| value.checked_div(parts.requests)),
                cost,
            }
        })
        .collect::<Vec<_>>();
    model_rows.sort_by(|left, right| {
        right
            .cost
            .estimated_usd_micros
            .cmp(&left.cost.estimated_usd_micros)
            .then_with(|| left.model.cmp(&right.model))
    });
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
                    .copied(),
                provenance: activity_provenance
                    .iter()
                    .filter(|((p, a, _), _)| p == &phase && a == &activity)
                    .map(|((_, _, provenance), count)| (provenance.clone(), *count))
                    .collect(),
                cost: cost_from_buckets(
                    activity_costs
                        .get(&(phase.clone(), activity.clone()))
                        .unwrap_or(&CostBuckets::default()),
                ),
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
    let mut outcome_rows = outcome_costs
        .iter()
        .map(|(outcome_id, bucket)| {
            let tokens = bucket.tokens;
            UsageOutcome {
                outcome_id: outcome_id.clone(),
                title: None,
                phase: None,
                activity: None,
                total_tokens: tokens,
                share: total_tokens
                    .filter(|total| *total > 0)
                    .map(|total| tokens as f64 / total as f64),
                operations: bucket.operations,
                cost: cost_from_buckets(bucket),
            }
        })
        .collect::<Vec<_>>();
    outcome_rows.sort_by(|left, right| {
        right
            .cost
            .estimated_usd_micros
            .cmp(&left.cost.estimated_usd_micros)
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
    let snapshots = reports
        .iter()
        .filter_map(|(_, report)| report.snapshot.as_ref())
        .collect::<Vec<_>>();
    let pending = failures
        .keys()
        .any(|reason| collector_api::is_pending(reason));
    let snapshot =
        (!snapshots.is_empty() || pending).then(|| devcoordinator2_api::results::UsageSnapshot {
            updated_at_ms: snapshots.iter().filter_map(|s| s.updated_at_ms).min(),
            refreshing: pending || snapshots.iter().any(|s| s.refreshing),
            refresh_failed: snapshots.iter().any(|s| s.refresh_failed),
            progress_completed: snapshots.iter().filter_map(|s| s.progress_completed).min(),
            progress_total: snapshots.iter().filter_map(|s| s.progress_total).max(),
            progress_stage: if pending {
                Some("source_read".into())
            } else {
                snapshots.iter().find_map(|s| s.progress_stage.clone())
            },
        });
    UsageRepository {
        repository_id: repository.repository_id.clone(),
        display_name: repository.display_name.clone(),
        range,
        generated_at_ms: now_ms,
        coverage: UsageCoverage {
            sources: reports
                .iter()
                .filter_map(|(_, report)| report.source_snapshot.clone())
                .collect(),
            snapshot,
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
            cost: total_cost,
        },
        series: points,
        activities: activity_rows,
        models: model_rows,
        outcomes: outcome_rows,
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
        worktree_scope: None,
    }
}

pub(crate) struct UserRecord {
    pub(crate) name: OsString,
    pub(crate) home: OsString,
    pub(crate) gid: u32,
}

pub(crate) fn user_record(uid: u32) -> io::Result<UserRecord> {
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

pub(crate) struct ProbeOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_truncated: bool,
}

pub(crate) fn run_probe_with_limit(
    mut command: Command,
    deadline: Instant,
    output_limit: usize,
) -> Result<ProbeOutput, String> {
    if Instant::now() >= deadline {
        return Err("query_budget_exhausted".into());
    }
    command.process_group(0);
    let mut child = command.spawn().map_err(|_| "source_unavailable")?;
    let Some(stdout) = child.stdout.take() else {
        kill_probe_group(&mut child);
        return Err("source_unavailable".into());
    };
    let Some(stderr) = child.stderr.take() else {
        kill_probe_group(&mut child);
        return Err("source_unavailable".into());
    };
    let output_overflow = Arc::new(AtomicBool::new(false));
    let stdout_complete = Arc::new(AtomicBool::new(false));
    let stderr_complete = Arc::new(AtomicBool::new(false));
    let stdout = {
        let output_overflow = output_overflow.clone();
        let complete = stdout_complete.clone();
        thread::spawn(move || read_capture(stdout, output_limit, output_overflow, complete))
    };
    let stderr = {
        let output_overflow = output_overflow.clone();
        let complete = stderr_complete.clone();
        thread::spawn(move || read_capture(stderr, output_limit, output_overflow, complete))
    };
    let status = loop {
        if output_overflow.load(Ordering::Acquire) {
            kill_probe_group(&mut child);
            let _ = stdout.join();
            let _ = stderr.join();
            return Err("source_unavailable".into());
        }
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => {
                kill_probe_group(&mut child);
                let _ = stdout.join();
                let _ = stderr.join();
                return Err("source_unavailable".into());
            }
        };
        if let Some(status) = child_status
            && stdout_complete.load(Ordering::Acquire)
            && stderr_complete.load(Ordering::Acquire)
        {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            kill_probe_group(&mut child);
            let _ = stdout.join();
            let _ = stderr.join();
            return Err("query_budget_exhausted".into());
        }
        thread::sleep(PROCESS_POLL.min(deadline.saturating_duration_since(now)));
    };
    let (stdout, stdout_truncated) = stdout
        .join()
        .map_err(|_| "source_unavailable")?
        .map_err(|_| "source_unavailable")?;
    let (_stderr, _stderr_truncated) = stderr
        .join()
        .map_err(|_| "source_unavailable")?
        .map_err(|_| "source_unavailable")?;
    Ok(ProbeOutput {
        status,
        stdout,
        stdout_truncated,
    })
}

fn kill_probe_group(child: &mut std::process::Child) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32);
    if let Some(pid) = pid {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
        thread::sleep(PROCESS_POLL);
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn read_capture(
    mut source: impl Read,
    limit: usize,
    output_overflow: Arc<AtomicBool>,
    complete: Arc<AtomicBool>,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            complete.store(true, Ordering::Release);
            return Ok((output, truncated));
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            truncated = true;
            output_overflow.store(true, Ordering::Release);
            complete.store(true, Ordering::Release);
            return Ok((output, truncated));
        }
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

#[cfg(test)]
fn bucket_index(at: u64, start: u64, bucket: u64, count: usize) -> Option<usize> {
    let index = at.checked_sub(start)? / bucket;
    usize::try_from(index).ok().filter(|index| *index < count)
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

#[cfg(test)]
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

#[cfg(test)]
fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
fn i64_value(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "source_unavailable".into())
}

fn valid_repository_key(value: &str) -> bool {
    value.strip_prefix("repo_").is_some_and(|digest| {
        digest.len() == 40 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
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

#[cfg(test)]
fn safe_coverage(value: &str) -> CoverageState {
    match value {
        "complete" => CoverageState::Complete,
        "unavailable" => CoverageState::Unavailable,
        "unobserved" => CoverageState::Unobserved,
        _ => CoverageState::Partial,
    }
}

#[cfg(test)]
fn coverage_name(value: &CoverageState) -> &'static str {
    match value {
        CoverageState::Complete => "complete",
        CoverageState::Partial => "partial",
        CoverageState::Unavailable => "unavailable",
        CoverageState::Unobserved => "unobserved",
    }
}

#[cfg(test)]
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

#[cfg(test)]
fn increment_pair(values: &mut BTreeMap<(String, String), u64>, key: (&str, &str), amount: u64) {
    let value = values.entry((key.0.into(), key.1.into())).or_default();
    *value = value.saturating_add(amount);
}

#[cfg(test)]
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
    use super::cost::{CostModel, select_card};
    use super::*;

    #[test]
    fn cached_cost_projection_prices_model_dimensions() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE _usage_report_dimension_tokens(
                    hour_index INTEGER NOT NULL,
                    repository_bucket TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    model TEXT NOT NULL,
                    provider_kind TEXT NOT NULL,
                    native_project_id TEXT NOT NULL,
                    workstream_id TEXT NOT NULL,
                    outcome_id TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    activity TEXT NOT NULL,
                    provenance TEXT NOT NULL,
                    category_path TEXT NOT NULL,
                    measurement_provenance TEXT NOT NULL,
                    coverage_state TEXT NOT NULL,
                    measured_tokens INTEGER NOT NULL,
                    unknown_observations INTEGER NOT NULL,
                    observation_count INTEGER NOT NULL,
                    aggregate_overflow INTEGER NOT NULL,
                    PRIMARY KEY(hour_index,repository_bucket,thread_id,model,provider_kind,native_project_id,workstream_id,outcome_id,phase,activity,provenance,category_path,measurement_provenance,coverage_state)
                );
                INSERT INTO _usage_report_dimension_tokens VALUES
                    (1,'repo-1','thread','gpt-6-sol','openai','','','','implementation','coding','agent_declared','input_tokens','provider_reported','complete',1000,0,1,0),
                    (1,'repo-1','thread','gpt-6-sol','openai','','','','implementation','coding','agent_declared','input_tokens_details.cached_tokens','provider_reported','complete',400,0,1,0),
                    (1,'repo-1','thread','gpt-6-sol','openai','','','','implementation','coding','agent_declared','input_tokens_details.cache_write_tokens','provider_reported','complete',0,0,1,0),
                    (1,'repo-1','thread','gpt-6-sol','openai','','','','implementation','coding','agent_declared','output_tokens','provider_reported','complete',100,0,1,0),
                    (1,'repo-1','thread','gpt-6-sol','openai','','','','implementation','coding','agent_declared','total_tokens','provider_reported','complete',1100,0,1,0);",
            )
            .unwrap();
        let card = devcoordinator2_api::rate_card::RateCard {
            card_id: "test-gpt-6-sol".into(),
            version: 1,
            provider: "openai".into(),
            model_pattern: "gpt-6-sol".into(),
            processing_tier: "standard".into(),
            context_tier: "short".into(),
            effective_from_ms: 0,
            effective_to_ms: None,
            input_usd_micros_per_million: 1_000_000,
            cached_input_usd_micros_per_million: 1_000_000,
            cache_write_usd_micros_per_million: 1_000_000,
            output_usd_micros_per_million: 1_000_000,
            source_ref: "fixture".into(),
            active: true,
        };
        let mut report = SourceReport {
            phase_series: vec![BTreeMap::new()],
            token_buckets_observed: vec![false],
            bucket_coverage: vec![CoverageState::Unobserved],
            ..Default::default()
        };
        add_cached_dimension_costs(
            &connection,
            &mut report,
            &["repo-1".into()],
            3_600_000,
            7_200_000,
            &[card],
        )
        .unwrap();
        assert_eq!(report.model_request_count, 1);
        assert_eq!(report.tokens.get("total_tokens"), Some(&1_100));
        assert_eq!(report.tokens.get("input_tokens"), Some(&1_000));
        let cost = cost_from_buckets(
            report
                .activity_costs
                .get(&("implementation".into(), "coding".into()))
                .unwrap(),
        );
        assert_eq!(cost.status, "complete");
        assert_eq!(cost.estimated_usd_micros, Some(1_100));
        assert_eq!(cost.model_requests, 1);
    }

    #[test]
    fn api_equivalent_cost_uses_mutually_exclusive_standard_token_components() {
        let mut buckets = CostBuckets::default();
        buckets.models.insert(
            "gpt-6-sol".into(),
            CostModel {
                amounts: [
                    1_300_000_000_000,
                    50_000_000_000,
                    250_000_000_000,
                    500_000_000_000,
                ],
                known_components: [true, true, true, true],
                cards: BTreeMap::from([(
                    "openai-standard-gpt-6-sol@1".into(),
                    devcoordinator2_api::rate_card::RateCard {
                        card_id: "openai-standard-gpt-6-sol".into(),
                        version: 1,
                        provider: "openai".into(),
                        model_pattern: "gpt-6-sol".into(),
                        processing_tier: "standard".into(),
                        context_tier: "short".into(),
                        effective_from_ms: 0,
                        effective_to_ms: None,
                        input_usd_micros_per_million: 2_000_000,
                        cached_input_usd_micros_per_million: 200_000,
                        cache_write_usd_micros_per_million: 2_500_000,
                        output_usd_micros_per_million: 10_000_000,
                        source_ref: "fixture".into(),
                        active: true,
                    },
                )]),
                tokens: 1_300_000,
                requests: 1,
                ..Default::default()
            },
        );
        let cost = cost_from_buckets(&buckets);
        assert_eq!(cost.status, "complete");
        assert_eq!(cost.estimated_usd_micros, Some(2_100_000));
        assert_eq!(cost.input_usd_micros, Some(1_300_000));
        assert_eq!(cost.cached_input_usd_micros, Some(50_000));
        assert_eq!(cost.cache_write_usd_micros, Some(250_000));
        assert_eq!(cost.output_usd_micros, Some(500_000));
        assert_eq!(cost.basis, "api_equivalent");
    }

    #[test]
    fn api_equivalent_cost_is_partial_for_unknown_models() {
        let mut buckets = CostBuckets::default();
        buckets.models.insert(
            "unknown-model".into(),
            CostModel {
                unknown_requests: 1,
                unknown_tokens: 10,
                tokens: 10,
                ..Default::default()
            },
        );
        let cost = cost_from_buckets(&buckets);
        assert_eq!(cost.status, "unavailable");
        assert_eq!(cost.estimated_usd_micros, None);
        assert!(cost.unknown_observations > 0);
    }

    #[test]
    fn api_equivalent_cost_selects_the_effective_rate_card_revision() {
        let cards = vec![
            devcoordinator2_api::rate_card::RateCard {
                card_id: "standard-sol".into(),
                version: 1,
                provider: "openai".into(),
                model_pattern: "gpt-custom".into(),
                processing_tier: "standard".into(),
                context_tier: "short".into(),
                effective_from_ms: 0,
                effective_to_ms: Some(2_000),
                input_usd_micros_per_million: 1,
                cached_input_usd_micros_per_million: 1,
                cache_write_usd_micros_per_million: 1,
                output_usd_micros_per_million: 1,
                source_ref: "https://example.test/v1".into(),
                active: true,
            },
            devcoordinator2_api::rate_card::RateCard {
                card_id: "standard-sol".into(),
                version: 2,
                provider: "openai".into(),
                model_pattern: "gpt-custom".into(),
                processing_tier: "standard".into(),
                context_tier: "short".into(),
                effective_from_ms: 2_000,
                effective_to_ms: None,
                input_usd_micros_per_million: 2,
                cached_input_usd_micros_per_million: 2,
                cache_write_usd_micros_per_million: 2,
                output_usd_micros_per_million: 2,
                source_ref: "https://example.test/v2".into(),
                active: true,
            },
        ];
        assert_eq!(
            select_card(&cards, "openai", "gpt-custom", 1, 1_999)
                .unwrap()
                .version,
            1
        );
        assert_eq!(
            select_card(&cards, "openai", "gpt-custom", 1, 2_000)
                .unwrap()
                .version,
            2
        );
        assert!(select_card(&cards, "openai", "other", 1, 2_000).is_none());
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
