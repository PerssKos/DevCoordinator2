//! Factual delivery progress from plan events, test history, and Codex usage.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use devcoordinator2_api::params::{
    ProgressPeriod, ProgressRepository as ProgressRepositoryParams, UsageRange,
};
use devcoordinator2_api::results::{
    CoverageState, Forecast, ForecastVelocity, NextRelease, PlanCoverage, ProgressComparison,
    ProgressCoverage, ProgressRepositories, ProgressRepository, ProgressRepositoryRow,
    ProgressScope, ProgressSemantics, ProgressSeriesPoint, ProgressTotals, ProgressWindow,
    ReleaseWork, TestCoverage, TestStatus, TokenCoverage,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use time::format_description::well_known::Rfc3339;

use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;
use crate::test_state::{TestHistoryEntry, TestRunStore};
use crate::usage::{CodexUsage, RepositoryRecord};

const HOUR_MS: u64 = 60 * 60 * 1_000;
const DAY_MS: u64 = 24 * HOUR_MS;
const WEEK_MS: u64 = 7 * DAY_MS;
const MONDAY_EPOCH_MS: u64 = 4 * DAY_MS;
const FORECAST_LOOKBACK_MS: u64 = 28 * DAY_MS;

#[derive(Clone)]
pub struct ProgressService {
    database: Database,
    registry: Registry,
    usage: CodexUsage,
    store: TestRunStore,
    clock: Arc<dyn Clock>,
}

#[derive(Clone)]
struct Repository {
    id: String,
    name: String,
    root: PathBuf,
    worktrees: Vec<PathBuf>,
}

#[derive(Clone)]
struct Task {
    id: String,
    seq: u32,
    position: i64,
    parent_id: Option<String>,
    release_id: Option<String>,
    title: String,
    kind: String,
    status: String,
    estimated_loc: Option<u32>,
    elaboration_needed: bool,
}

#[derive(Clone)]
struct Release {
    id: String,
    name: String,
    status: String,
}

#[derive(Clone)]
struct PlanEvent {
    subject_id: String,
    event: String,
    from_value: Option<String>,
    to_value: Option<String>,
    at_ms: Option<u64>,
}

struct PlanSeries {
    tasks: Vec<Task>,
    parents: HashSet<String>,
    leaves: Vec<Task>,
    completed_with_estimate: u32,
    completed_total: u32,
}

#[derive(Clone)]
struct Bucket {
    start: u64,
    end: u64,
    tasks_completed: u32,
    tasks_created: u32,
    tasks_reopened: u32,
    planned_lines_completed: i64,
    planned_lines_added: i64,
    scope_lines_changed: i64,
    test_runs: u32,
    tests_passed: u32,
    test_pass_rate: Option<f64>,
    total_tokens: Option<u64>,
    token_coverage: CoverageState,
}

struct Window {
    bucket_ms: u64,
    current_buckets: usize,
    total_buckets: usize,
    aligned_end_ms: u64,
    start_ms: u64,
    current_start_ms: u64,
}

impl ProgressService {
    pub fn new(database: Database, registry: Registry, usage: CodexUsage) -> Self {
        Self::with_clock(database, registry, usage, Arc::new(HostClock))
    }

    pub fn with_clock(
        database: Database,
        registry: Registry,
        usage: CodexUsage,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            database,
            registry,
            usage,
            store: TestRunStore,
            clock,
        }
    }

    pub fn repositories(&self) -> Result<ProgressRepositories, ProtocolError> {
        let repositories = self.repository_records()?;
        let mut rows = Vec::new();
        for repository in repositories {
            let (tasks, parents) = self.task_rows(&repository.id)?;
            let leaves = tasks
                .iter()
                .filter(|task| task.status != "dropped" && !parents.contains(&task.id))
                .collect::<Vec<_>>();
            let release = self.next_release(&repository.id)?;
            rows.push(ProgressRepositoryRow {
                repository_id: repository.id,
                display_name: repository.name,
                open_tasks: count_u32(leaves.iter().filter(|task| task.status != "done").count()),
                tasks_done: count_u32(leaves.iter().filter(|task| task.status == "done").count()),
                planned_lines_done: leaves
                    .iter()
                    .filter(|task| task.status == "done")
                    .map(|task| u64::from(task.estimated_loc.unwrap_or(0)))
                    .sum(),
                planned_lines_total: leaves
                    .iter()
                    .map(|task| u64::from(task.estimated_loc.unwrap_or(0)))
                    .sum(),
                next_release: release.as_ref().map(api_release).transpose()?,
            });
        }
        Ok(ProgressRepositories { repositories: rows })
    }

    pub fn repository(
        &self,
        params: ProgressRepositoryParams,
    ) -> Result<ProgressRepository, ProtocolError> {
        let repository = self
            .repository_records()?
            .into_iter()
            .find(|repository| repository.id == params.repository_id)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::RepositoryNotFound, "no registered repository")
            })?;
        let report = self.repository_at(&repository, params.period.clone(), self.now_ms()?)?;
        if params.wait_for_refresh {
            self.usage.wait_for_refresh(Some(&repository.id));
            return self.repository_at(&repository, params.period, self.now_ms()?);
        }
        Ok(report)
    }

    fn repository_at(
        &self,
        repository: &Repository,
        period: ProgressPeriod,
        now_ms: u64,
    ) -> Result<ProgressRepository, ProtocolError> {
        let window = progress_window(&period, now_ms);
        let mut buckets = empty_buckets(&window);
        let plan = self.plan_series(&repository.id, &window, &mut buckets)?;
        let (test_runs, mut test_coverage) = self.test_history(repository)?;
        if test_coverage.state == CoverageState::Complete
            && test_coverage
                .earliest_at
                .as_deref()
                .and_then(parse_ms)
                .unwrap_or(now_ms)
                > window.start_ms
        {
            test_coverage.state = CoverageState::Partial;
        }
        add_test_series(&test_runs, &window, &mut buckets);
        let usage_repository = RepositoryRecord {
            repository_id: repository.id.clone(),
            display_name: repository.name.clone(),
            root_path: repository.root.clone(),
        };
        let usage = self.usage.repository_buckets(
            &usage_repository,
            period_usage_range(&period),
            window.bucket_ms,
            window.total_buckets,
            now_ms,
            window.aligned_end_ms,
            true,
        )?;
        for (bucket, point) in buckets.iter_mut().zip(&usage.series) {
            bucket.token_coverage = point.coverage.clone();
            bucket.total_tokens = point.total_tokens;
        }
        let split = window.current_buckets;
        let (previous_buckets, current_buckets) = buckets.split_at(split);
        let duration_days = split as f64 * window.bucket_ms as f64 / DAY_MS as f64;
        let current = totals(current_buckets, duration_days);
        let previous = totals(previous_buckets, duration_days);
        let (release, release_scope) =
            self.release_scope(&repository.id, &plan.tasks, &plan.parents)?;
        let evidence =
            self.completion_evidence(&repository.id, &plan.tasks, &plan.parents, now_ms)?;
        let forecast = forecast(
            &release_scope,
            &evidence,
            now_ms,
            current.test_pass_rate,
            current.scope_lines_changed,
            release.as_ref(),
        )?;
        let release_work = self.release_work(&repository.id, &release_scope)?;
        let leaves = plan
            .leaves
            .iter()
            .filter(|task| task.status != "dropped")
            .collect::<Vec<_>>();
        let token_coverage = TokenCoverage {
            snapshot: usage.coverage.snapshot.clone(),
            state: usage.coverage.state.clone(),
            has_gaps: usage.coverage.has_gaps,
            configured_collectors: usage.coverage.configured_collectors,
            available_collectors: usage.coverage.available_collectors,
            contributing_collectors: usage.coverage.contributing_collectors,
            freshest_at_ms: usage.coverage.freshest_at_ms,
            unavailable_reasons: usage.coverage.unavailable_reasons,
        };
        let overall_coverage = if test_coverage.state == CoverageState::Complete
            && token_coverage.state == CoverageState::Complete
        {
            CoverageState::Complete
        } else if matches!(
            test_coverage.state,
            CoverageState::Unavailable | CoverageState::Unobserved
        ) && matches!(
            token_coverage.state,
            CoverageState::Unavailable | CoverageState::Unobserved
        ) {
            CoverageState::Unavailable
        } else {
            CoverageState::Partial
        };
        Ok(ProgressRepository {
            repository_id: repository.id.clone(),
            display_name: repository.name.clone(),
            period,
            generated_at_ms: now_ms,
            window: ProgressWindow {
                bucket_ms: window.bucket_ms,
                start_ms: window.current_start_ms,
                end_ms: now_ms,
                comparison_start_ms: window.start_ms,
                timezone: "UTC".into(),
            },
            scope: ProgressScope {
                tasks_total: count_u32(leaves.len()),
                tasks_done: count_u32(leaves.iter().filter(|task| task.status == "done").count()),
                planned_lines_total: leaves
                    .iter()
                    .map(|task| u64::from(task.estimated_loc.unwrap_or(0)))
                    .sum(),
                planned_lines_done: leaves
                    .iter()
                    .filter(|task| task.status == "done")
                    .map(|task| u64::from(task.estimated_loc.unwrap_or(0)))
                    .sum(),
                unestimated_open_tasks: count_u32(
                    leaves
                        .iter()
                        .filter(|task| task.status != "done" && task.estimated_loc.is_none())
                        .count(),
                ),
            },
            series: current_buckets.iter().map(api_bucket).collect(),
            comparison: ProgressComparison { current, previous },
            forecast,
            release_work,
            coverage: ProgressCoverage {
                state: overall_coverage,
                plan: PlanCoverage {
                    state: CoverageState::Complete,
                    completed_with_estimate: plan.completed_with_estimate,
                    completed_total: plan.completed_total,
                },
                tests: test_coverage,
                tokens: token_coverage,
            },
            semantics: ProgressSemantics {
                tasks: "terminal task status events in the permanent plan ledger".into(),
                lines: "current planned task estimates completed; not measured Git changes"
                    .into(),
                lines_added: "initial task estimates and estimate increases; estimate reductions and dropped work are excluded".into(),
                tests: "bounded repository-local terminal test summaries".into(),
                tokens: "provider total_tokens; missing collector coverage stays missing".into(),
                forecast: "provisional range from recent completion pace, current estimates, scope movement, and test stability; unestimated work uses the median recorded task size when available".into(),
            },
        })
    }

    fn now_ms(&self) -> Result<u64, ProtocolError> {
        u64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000_000).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "progress timestamp is before Unix epoch",
            )
        })
    }

    fn repository_records(&self) -> Result<Vec<Repository>, ProtocolError> {
        Ok(self
            .registry
            .list_repositories(false)?
            .repositories
            .into_iter()
            .map(|repository| Repository {
                id: repository.repository_id,
                name: repository.display_name,
                root: repository.root_path.into(),
                worktrees: repository
                    .worktrees
                    .into_iter()
                    .map(|worktree| PathBuf::from(worktree.worktree_path))
                    .collect(),
            })
            .collect())
    }

    fn task_rows(
        &self,
        repository_id: &str,
    ) -> Result<(Vec<Task>, HashSet<String>), ProtocolError> {
        let repository_id = repository_id.to_owned();
        let tasks = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT task_id,seq,position,parent_task_id,release_id,title,kind,status,estimated_loc,elaboration_needed FROM tasks WHERE repository_id=?1 ORDER BY seq",
                )?;
                Ok(statement
                    .query_map([repository_id], |row| {
                        Ok(Task {
                            id: row.get(0)?,
                            seq: row.get(1)?,
                            position: row.get(2)?,
                            parent_id: row.get(3)?,
                            release_id: row.get(4)?,
                            title: row.get(5)?,
                            kind: row.get(6)?,
                            status: row.get(7)?,
                            estimated_loc: row.get(8)?,
                            elaboration_needed: row.get::<_, i64>(9)? != 0,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        let parents = tasks
            .iter()
            .filter_map(|task| task.parent_id.clone())
            .collect();
        Ok((tasks, parents))
    }

    fn plan_series(
        &self,
        repository_id: &str,
        window: &Window,
        buckets: &mut [Bucket],
    ) -> Result<PlanSeries, ProtocolError> {
        let (tasks, parents) = self.task_rows(repository_id)?;
        let by_id = tasks
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect::<HashMap<_, _>>();
        let repository_id = repository_id.to_owned();
        let start = iso(window.start_ms)?;
        let end = iso(window.aligned_end_ms)?;
        let events = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT subject_id,event,from_value,to_value,at FROM plan_events WHERE repository_id=?1 AND subject_kind='task' AND at>=?2 AND at<?3 ORDER BY event_id",
                )?;
                Ok(statement
                    .query_map(rusqlite::params![repository_id,start,end], |row| {
                        let at: String = row.get(4)?;
                        Ok(PlanEvent {
                            subject_id: row.get(0)?,
                            event: row.get(1)?,
                            from_value: row.get(2)?,
                            to_value: row.get(3)?,
                            at_ms: parse_ms(&at),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        let mut initial_estimates = HashMap::new();
        for event in &events {
            if event.event == "estimate" {
                initial_estimates
                    .entry(event.subject_id.clone())
                    .or_insert_with(|| number(event.from_value.as_deref()));
            }
        }
        let mut completed_with_estimate = 0_u32;
        let mut completed_total = 0_u32;
        for event in events {
            let Some(task) = by_id.get(event.subject_id.as_str()) else {
                continue;
            };
            if parents.contains(&task.id) {
                continue;
            }
            let Some(index) = event.at_ms.and_then(|at| bucket_index(at, window)) else {
                continue;
            };
            let bucket = &mut buckets[index];
            let estimate = i64::from(task.estimated_loc.unwrap_or(0));
            match (
                event.event.as_str(),
                event.from_value.as_deref(),
                event.to_value.as_deref(),
            ) {
                ("created", _, _) => {
                    let initial = initial_estimates.get(&task.id).copied().unwrap_or(estimate);
                    bucket.tasks_created = bucket.tasks_created.saturating_add(1);
                    bucket.planned_lines_added = bucket.planned_lines_added.saturating_add(initial);
                    bucket.scope_lines_changed = bucket.scope_lines_changed.saturating_add(initial);
                }
                ("status", _, Some("done")) => {
                    bucket.tasks_completed = bucket.tasks_completed.saturating_add(1);
                    bucket.planned_lines_completed =
                        bucket.planned_lines_completed.saturating_add(estimate);
                    completed_total = completed_total.saturating_add(1);
                    completed_with_estimate =
                        completed_with_estimate.saturating_add(u32::from(estimate > 0));
                }
                ("status", Some("done"), Some("in_progress")) => {
                    bucket.tasks_reopened = bucket.tasks_reopened.saturating_add(1);
                }
                ("status", _, Some("dropped")) => {
                    bucket.scope_lines_changed =
                        bucket.scope_lines_changed.saturating_sub(estimate);
                }
                ("status", Some("dropped"), _) => {
                    bucket.scope_lines_changed =
                        bucket.scope_lines_changed.saturating_add(estimate);
                }
                ("estimate", _, _) => {
                    let delta = number(event.to_value.as_deref())
                        .saturating_sub(number(event.from_value.as_deref()));
                    bucket.planned_lines_added =
                        bucket.planned_lines_added.saturating_add(delta.max(0));
                    bucket.scope_lines_changed = bucket.scope_lines_changed.saturating_add(delta);
                }
                _ => {}
            }
        }
        let active = tasks
            .iter()
            .filter(|task| task.status != "dropped")
            .cloned()
            .collect::<Vec<_>>();
        let leaves = active
            .iter()
            .filter(|task| !parents.contains(&task.id))
            .cloned()
            .collect();
        Ok(PlanSeries {
            tasks,
            parents,
            leaves,
            completed_with_estimate,
            completed_total,
        })
    }

    fn test_history(
        &self,
        repository: &Repository,
    ) -> Result<(Vec<TestHistoryEntry>, TestCoverage), ProtocolError> {
        let mut runs = BTreeMap::new();
        let mut unavailable = 0_u32;
        let mut sources = 0_u32;
        for worktree in &repository.worktrees {
            match self.store.read_history(worktree) {
                Ok(history) => {
                    sources = sources.saturating_add(u32::from(!history.is_empty()));
                    for run in history {
                        runs.insert(run.run_id.clone(), run);
                    }
                }
                Err(_) => unavailable = unavailable.saturating_add(1),
            }
            if let Ok(Some(summary)) = self.store.read_current_summary(worktree)
                && summary.status != TestStatus::Running
            {
                runs.entry(summary.run_id.clone())
                    .or_insert(TestHistoryEntry {
                        work: summary.work,
                        run_id: summary.run_id,
                        test: summary.test,
                        status: summary.status,
                        started_at: summary.started_at,
                        finished_at: summary.finished_at,
                        duration_seconds: summary.duration_seconds,
                        exit_code: summary.exit_code,
                        termination_reason: summary.termination_reason,
                    });
            }
        }
        let mut runs = runs.into_values().collect::<Vec<_>>();
        runs.sort_by(|left, right| left.finished_at.cmp(&right.finished_at));
        let state = if unavailable > 0 && runs.is_empty() {
            CoverageState::Unavailable
        } else if unavailable > 0 || (!runs.is_empty() && sources == 0) {
            CoverageState::Partial
        } else if runs.is_empty() {
            CoverageState::Unobserved
        } else {
            CoverageState::Complete
        };
        let coverage = TestCoverage {
            state,
            recorded_runs: count_u32(runs.len()),
            history_sources: sources,
            unavailable_sources: unavailable,
            earliest_at: runs.first().and_then(|run| run.finished_at.clone()),
        };
        Ok((runs, coverage))
    }

    fn next_release(&self, repository_id: &str) -> Result<Option<Release>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT release_id,name,status FROM releases WHERE repository_id=?1 AND status IN ('planned','requested') ORDER BY seq LIMIT 1",
                        [repository_id],
                        |row| Ok(Release { id:row.get(0)?,name:row.get(1)?,status:row.get(2)? }),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    fn release_scope(
        &self,
        repository_id: &str,
        tasks: &[Task],
        parents: &HashSet<String>,
    ) -> Result<(Option<Release>, Vec<Task>), ProtocolError> {
        let release = self.next_release(repository_id)?;
        let release_id = release.as_ref().map(|release| release.id.as_str());
        let grouped = tasks
            .iter()
            .filter(|task| task.status != "dropped" && task.release_id.as_deref() == release_id)
            .cloned()
            .collect::<Vec<_>>();
        let by_id = grouped
            .iter()
            .map(|task| (task.id.clone(), task.clone()))
            .collect::<HashMap<_, _>>();
        let mut children: HashMap<String, Vec<Task>> = HashMap::new();
        for task in &grouped {
            if let Some(parent) = task
                .parent_id
                .as_ref()
                .filter(|parent| by_id.contains_key(*parent))
            {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(task.clone());
            }
        }
        for children in children.values_mut() {
            children.sort_by_key(|task| (task.position, task.seq));
        }
        fn walk(
            task: &Task,
            children: &HashMap<String, Vec<Task>>,
            parents: &HashSet<String>,
            scope: &mut Vec<Task>,
        ) {
            if let Some(nested) = children.get(&task.id) {
                for child in nested {
                    walk(child, children, parents, scope);
                }
            } else if !parents.contains(&task.id) {
                scope.push(task.clone());
            }
        }
        let mut roots = grouped
            .iter()
            .filter(|task| {
                task.parent_id
                    .as_ref()
                    .is_none_or(|parent| !by_id.contains_key(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        roots.sort_by_key(|task| (task.position, task.seq));
        let mut scope = Vec::new();
        for root in roots {
            walk(&root, &children, parents, &mut scope);
        }
        Ok((release, scope))
    }

    fn completion_evidence(
        &self,
        repository_id: &str,
        tasks: &[Task],
        parents: &HashSet<String>,
        now_ms: u64,
    ) -> Result<ForecastVelocity, ProtocolError> {
        let cutoff = now_ms.saturating_sub(FORECAST_LOOKBACK_MS);
        let repository_id = repository_id.to_owned();
        let events = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT subject_id,at FROM plan_events WHERE repository_id=?1 AND subject_kind='task' AND event='status' AND to_value='done' AND at>=?2 AND at<?3 ORDER BY event_id",
                )?;
                Ok(statement
                    .query_map(
                        rusqlite::params![repository_id,iso(cutoff).map_err(DatabaseError::Domain)?,iso(now_ms.saturating_add(1000)).map_err(DatabaseError::Domain)?],
                        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)),
                    )?
                    .collect::<Result<Vec<_>,_>>()?)
            })
            .map_err(database_error)?;
        let by_id = tasks
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect::<HashMap<_, _>>();
        let completed = events
            .into_iter()
            .filter_map(|(task_id, at)| {
                let task = by_id.get(task_id.as_str())?;
                let at = parse_ms(&at)?;
                (at >= cutoff && !parents.contains(&task.id)).then_some((at, task.estimated_loc))
            })
            .collect::<Vec<_>>();
        if completed.is_empty() {
            return Ok(ForecastVelocity {
                tasks: 0,
                planned_lines: 0,
                lookback_days: 28.0,
                tasks_per_day: 0.0,
                planned_lines_per_day: 0.0,
            });
        }
        let earliest = completed.iter().map(|(at, _)| *at).min().unwrap_or(now_ms);
        let days =
            ((now_ms.saturating_sub(earliest)) as f64 / DAY_MS as f64 + 1.0).clamp(7.0, 28.0);
        let lines = completed
            .iter()
            .map(|(_, estimate)| u64::from(estimate.unwrap_or(0)))
            .sum::<u64>();
        Ok(ForecastVelocity {
            tasks: count_u32(completed.len()),
            planned_lines: lines,
            lookback_days: round(days, 2),
            tasks_per_day: completed.len() as f64 / days,
            planned_lines_per_day: lines as f64 / days,
        })
    }

    fn release_work(
        &self,
        repository_id: &str,
        scope: &[Task],
    ) -> Result<Vec<ReleaseWork>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        let reopened = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT subject_id FROM plan_events WHERE repository_id=?1 AND subject_kind='task' AND event='status' AND from_value='done' AND to_value!='done' ORDER BY event_id",
                )?;
                Ok(statement
                    .query_map([repository_id], |row| row.get::<_,String>(0))?
                    .collect::<Result<HashSet<_>,_>>()?)
            })
            .map_err(database_error)?;
        scope
            .iter()
            .filter(|task| task.status != "done")
            .map(|task| {
                Ok(ReleaseWork {
                    task_id: task.id.clone(),
                    title: task.title.clone(),
                    status: task_status(&task.status)?,
                    kind: task_kind(&task.kind)?,
                    estimated_loc: task.estimated_loc,
                    elaboration_needed: task.elaboration_needed,
                    // UIL-UI-PROGRESS-002: compact progress never exposes raw
                    // unblock or event-note prose. Plan remains the detail route.
                    unblock_condition: None,
                    reopened: reopened.contains(&task.id),
                    reopen_note: None,
                })
            })
            .collect()
    }
}

fn progress_window(period: &ProgressPeriod, now_ms: u64) -> Window {
    let (bucket_ms, current_buckets) = match period {
        ProgressPeriod::Hour => (HOUR_MS, 24),
        ProgressPeriod::Day => (DAY_MS, 7),
        ProgressPeriod::Week => (WEEK_MS, 8),
    };
    let aligned_end_ms = if matches!(period, ProgressPeriod::Week) {
        now_ms
            .saturating_sub(MONDAY_EPOCH_MS)
            .div_ceil(bucket_ms)
            .saturating_mul(bucket_ms)
            .saturating_add(MONDAY_EPOCH_MS)
    } else {
        now_ms.div_ceil(bucket_ms).saturating_mul(bucket_ms)
    };
    let total_buckets = current_buckets * 2;
    let start_ms = aligned_end_ms
        .saturating_sub(bucket_ms.saturating_mul(u64::try_from(total_buckets).unwrap_or(u64::MAX)));
    Window {
        bucket_ms,
        current_buckets,
        total_buckets,
        aligned_end_ms,
        start_ms,
        current_start_ms: aligned_end_ms.saturating_sub(
            bucket_ms.saturating_mul(u64::try_from(current_buckets).unwrap_or(u64::MAX)),
        ),
    }
}

fn empty_buckets(window: &Window) -> Vec<Bucket> {
    (0..window.total_buckets)
        .map(|index| Bucket {
            start: window.start_ms.saturating_add(
                window
                    .bucket_ms
                    .saturating_mul(u64::try_from(index).unwrap_or(u64::MAX)),
            ),
            end: window.start_ms.saturating_add(
                window
                    .bucket_ms
                    .saturating_mul(u64::try_from(index + 1).unwrap_or(u64::MAX)),
            ),
            tasks_completed: 0,
            tasks_created: 0,
            tasks_reopened: 0,
            planned_lines_completed: 0,
            planned_lines_added: 0,
            scope_lines_changed: 0,
            test_runs: 0,
            tests_passed: 0,
            test_pass_rate: None,
            total_tokens: None,
            token_coverage: CoverageState::Unobserved,
        })
        .collect()
}

fn add_test_series(runs: &[TestHistoryEntry], window: &Window, buckets: &mut [Bucket]) {
    for run in runs {
        if !matches!(
            run.status,
            TestStatus::Passed
                | TestStatus::Failed
                | TestStatus::TimedOut
                | TestStatus::Interrupted
        ) {
            continue;
        }
        let Some(index) = run
            .finished_at
            .as_deref()
            .and_then(parse_ms)
            .and_then(|at| bucket_index(at, window))
        else {
            continue;
        };
        buckets[index].test_runs = buckets[index].test_runs.saturating_add(1);
        buckets[index].tests_passed = buckets[index]
            .tests_passed
            .saturating_add(u32::from(run.status == TestStatus::Passed));
    }
    for bucket in buckets {
        if bucket.test_runs > 0 {
            bucket.test_pass_rate =
                Some(f64::from(bucket.tests_passed) / f64::from(bucket.test_runs));
        }
    }
}

fn totals(buckets: &[Bucket], duration_days: f64) -> ProgressTotals {
    let tests = buckets.iter().map(|bucket| bucket.test_runs).sum::<u32>();
    let passed = buckets
        .iter()
        .map(|bucket| bucket.tests_passed)
        .sum::<u32>();
    let tokens = buckets
        .iter()
        .filter_map(|bucket| bucket.total_tokens)
        .collect::<Vec<_>>();
    let lines = buckets
        .iter()
        .map(|bucket| bucket.planned_lines_completed)
        .sum::<i64>();
    let lines_added = buckets
        .iter()
        .map(|bucket| bucket.planned_lines_added)
        .sum::<i64>();
    let completed = buckets
        .iter()
        .map(|bucket| bucket.tasks_completed)
        .sum::<u32>();
    let created = buckets
        .iter()
        .map(|bucket| bucket.tasks_created)
        .sum::<u32>();
    let total_tokens = (!tokens.is_empty()).then(|| tokens.iter().copied().sum());
    ProgressTotals {
        tasks_completed: completed,
        tasks_created: created,
        tasks_reopened: buckets.iter().map(|bucket| bucket.tasks_reopened).sum(),
        planned_lines_completed: lines,
        planned_lines_added: lines_added,
        scope_lines_changed: buckets
            .iter()
            .map(|bucket| bucket.scope_lines_changed)
            .sum(),
        test_runs: tests,
        tests_passed: passed,
        test_pass_rate: (tests > 0).then(|| f64::from(passed) / f64::from(tests)),
        total_tokens,
        tokens_per_completed_task: total_tokens
            .filter(|_| completed > 0)
            .map(|tokens| tokens as f64 / f64::from(completed)),
        tokens_per_planned_line: total_tokens
            .filter(|_| lines > 0)
            .map(|tokens| tokens as f64 / lines as f64),
        tasks_completed_per_day: f64::from(completed) / duration_days,
        tasks_created_per_day: f64::from(created) / duration_days,
        tests_per_completed_task: (completed > 0).then(|| f64::from(tests) / f64::from(completed)),
    }
}

fn api_bucket(bucket: &Bucket) -> ProgressSeriesPoint {
    ProgressSeriesPoint {
        bucket_start_ms: bucket.start,
        bucket_end_ms: bucket.end,
        tasks_completed: bucket.tasks_completed,
        tasks_created: bucket.tasks_created,
        tasks_reopened: bucket.tasks_reopened,
        planned_lines_completed: bucket.planned_lines_completed,
        planned_lines_added: bucket.planned_lines_added,
        scope_lines_changed: bucket.scope_lines_changed,
        test_runs: bucket.test_runs,
        tests_passed: bucket.tests_passed,
        test_pass_rate: bucket.test_pass_rate,
        total_tokens: bucket.total_tokens,
        token_coverage: bucket.token_coverage.clone(),
    }
}

fn forecast(
    scope: &[Task],
    velocity: &ForecastVelocity,
    now_ms: u64,
    test_pass_rate: Option<f64>,
    scope_change: i64,
    release: Option<&Release>,
) -> Result<Forecast, ProtocolError> {
    let open = scope
        .iter()
        .filter(|task| task.status != "done")
        .collect::<Vec<_>>();
    let mut sizes = scope
        .iter()
        .filter_map(|task| task.estimated_loc.filter(|size| *size > 0))
        .collect::<Vec<_>>();
    sizes.sort_unstable();
    let remaining_lines = open
        .iter()
        .map(|task| u64::from(task.estimated_loc.unwrap_or(0)))
        .sum::<u64>();
    let unknown = count_u32(
        open.iter()
            .filter(|task| task.estimated_loc.is_none())
            .count(),
    );
    let release = release.map(api_release).transpose()?;
    let mut result = Forecast {
        release,
        remaining_tasks: count_u32(open.len()),
        remaining_planned_lines: remaining_lines,
        unestimated_tasks: unknown,
        velocity: velocity.clone(),
        target_date_recorded: false,
        as_of_ms: now_ms,
        state: String::new(),
        reason: None,
        likely_at_ms: None,
        earliest_at_ms: None,
        latest_at_ms: None,
        confidence_percent: None,
        confidence: None,
        drivers: None,
        explanation: String::new(),
        assumptions: None,
    };
    if result.release.is_none() {
        result.state = "unavailable".into();
        result.reason = Some("no_planned_release".into());
        result.explanation = "Plan a release before estimating its delivery range.".into();
        return Ok(result);
    }
    if open.is_empty() {
        result.state = "ready".into();
        result.likely_at_ms = Some(now_ms);
        result.earliest_at_ms = Some(now_ms);
        result.latest_at_ms = Some(now_ms);
        result.confidence_percent = Some(90);
        result.confidence = Some("high".into());
        result.explanation =
            "All recorded work for this release is complete. No target date is recorded.".into();
        return Ok(result);
    }
    let median = median(&sizes);
    let equivalent_lines = remaining_lines as f64 + f64::from(unknown) * median.unwrap_or(0.0);
    let days = if equivalent_lines > 0.0 && velocity.planned_lines_per_day > 0.0 {
        equivalent_lines / velocity.planned_lines_per_day
    } else if velocity.tasks_per_day > 0.0 {
        open.len() as f64 / velocity.tasks_per_day
    } else {
        result.state = "unavailable".into();
        result.reason = Some("insufficient_completion_history".into());
        result.explanation =
            "More completed work is needed before pace can support a release forecast.".into();
        return Ok(result);
    };
    let mut risk = 1.0;
    let mut drivers = Vec::new();
    if unknown > 0 {
        risk += (f64::from(unknown) / open.len().max(1) as f64 * 0.35).min(0.35);
        drivers.push(format!(
            "{unknown} remaining task{} not estimated",
            if unknown == 1 { " is" } else { "s are" }
        ));
    }
    if test_pass_rate.is_some_and(|rate| rate < 0.8) {
        risk += 0.15;
        drivers.push("recent test stability is below 80%".into());
    }
    if scope_change > 0 {
        risk += 0.1;
        drivers.push("measured scope grew during this period".into());
    }
    let likely_days = (days * risk).max(1.0);
    let mut confidence = 88_i64;
    if velocity.tasks < 5 {
        confidence -= 15;
    }
    if velocity.tasks < 2 {
        confidence -= 15;
    }
    confidence -= (f64::from(unknown) / open.len().max(1) as f64 * 30.0).round() as i64;
    confidence -= match test_pass_rate {
        None => 8,
        Some(rate) if rate < 0.8 => 12,
        Some(_) => 0,
    };
    if scope_change > 0 {
        confidence -= 8;
    }
    let confidence = u8::try_from(confidence.clamp(20, 90)).unwrap_or(20);
    let spread = (likely_days * ((100.0 - f64::from(confidence)) / 100.0 + 0.1))
        .ceil()
        .max(1.0) as u64;
    let likely_whole = likely_days.ceil().max(1.0) as u64;
    let earliest_days = (likely_days.floor().max(1.0) as u64)
        .saturating_sub(spread)
        .max(1);
    result.state = "available".into();
    result.likely_at_ms = Some(now_ms.saturating_add(likely_whole.saturating_mul(DAY_MS)));
    result.earliest_at_ms = Some(now_ms.saturating_add(earliest_days.saturating_mul(DAY_MS)));
    result.latest_at_ms =
        Some(now_ms.saturating_add(likely_whole.saturating_add(spread).saturating_mul(DAY_MS)));
    result.confidence_percent = Some(confidence);
    result.confidence = Some(
        if confidence >= 80 {
            "high"
        } else if confidence >= 55 {
            "medium"
        } else {
            "low"
        }
        .into(),
    );
    result.explanation = if let Some(driver) = drivers.first() {
        let mut chars = driver.chars();
        format!(
            "{}{}. No target date is recorded.",
            chars
                .next()
                .map(|character| character.to_ascii_uppercase())
                .unwrap_or_default(),
            chars.as_str()
        )
    } else {
        "The range is based on recent measured completion pace. No target date is recorded.".into()
    };
    result.drivers = Some(drivers);
    result.assumptions = Some(vec![
        "Current task estimates are used as planned size, not measured Git changes.".into(),
        "Unestimated work uses the median recorded task size when available.".into(),
        "Ordering alone does not change total scope or the central forecast.".into(),
    ]);
    Ok(result)
}

fn median(values: &[u32]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((f64::from(values[middle - 1]) + f64::from(values[middle])) / 2.0)
    } else {
        Some(f64::from(values[middle]))
    }
}

fn period_usage_range(period: &ProgressPeriod) -> UsageRange {
    match period {
        ProgressPeriod::Hour => UsageRange::Hours24,
        ProgressPeriod::Day => UsageRange::Days7,
        ProgressPeriod::Week => UsageRange::Days30,
    }
}

fn api_release(release: &Release) -> Result<NextRelease, ProtocolError> {
    Ok(NextRelease {
        release_id: release.id.clone(),
        name: release.name.clone(),
        status: release_status(&release.status)?,
    })
}

fn release_status(
    value: &str,
) -> Result<devcoordinator2_api::params::ReleaseStatus, ProtocolError> {
    match value {
        "planned" => Ok(devcoordinator2_api::params::ReleaseStatus::Planned),
        "requested" => Ok(devcoordinator2_api::params::ReleaseStatus::Requested),
        "delivered" => Ok(devcoordinator2_api::params::ReleaseStatus::Delivered),
        "dropped" => Ok(devcoordinator2_api::params::ReleaseStatus::Dropped),
        _ => Err(stored_value_error("release status")),
    }
}

fn task_status(value: &str) -> Result<devcoordinator2_api::params::TaskStatus, ProtocolError> {
    match value {
        "planned" => Ok(devcoordinator2_api::params::TaskStatus::Planned),
        "in_progress" => Ok(devcoordinator2_api::params::TaskStatus::InProgress),
        "done" => Ok(devcoordinator2_api::params::TaskStatus::Done),
        "dropped" => Ok(devcoordinator2_api::params::TaskStatus::Dropped),
        _ => Err(stored_value_error("task status")),
    }
}

fn task_kind(value: &str) -> Result<devcoordinator2_api::params::TaskKind, ProtocolError> {
    match value {
        "goal" => Ok(devcoordinator2_api::params::TaskKind::Goal),
        "stub" => Ok(devcoordinator2_api::params::TaskKind::Stub),
        "improvement" => Ok(devcoordinator2_api::params::TaskKind::Improvement),
        "user_feedback" => Ok(devcoordinator2_api::params::TaskKind::UserFeedback),
        _ => Err(stored_value_error("task kind")),
    }
}

fn stored_value_error(label: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        format!("stored {label} is invalid"),
    )
}

fn parse_ms(value: &str) -> Option<u64> {
    let timestamp = time::OffsetDateTime::parse(value, &Rfc3339).ok()?;
    u64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).ok()
}

fn iso(milliseconds: u64) -> Result<String, ProtocolError> {
    let nanos = i128::from(milliseconds).saturating_mul(1_000_000);
    let timestamp = time::OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot format progress range")
            .with_detail(error.to_string())
    })?;
    timestamp.format(&Rfc3339).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot format progress range")
            .with_detail(error.to_string())
    })
}

fn bucket_index(at: u64, window: &Window) -> Option<usize> {
    let index = at.checked_sub(window.start_ms)? / window.bucket_ms;
    usize::try_from(index)
        .ok()
        .filter(|index| *index < window.total_buckets)
}

fn number(value: Option<&str>) -> i64 {
    value
        .filter(|value| !value.is_empty() && *value != "None")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn count_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10_f64.powi(places);
    (value * scale).round() / scale
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "progress query failed")
            .with_detail(other.to_string()),
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::platform::FixedClock;
    use crate::test_state::{initial_summary, terminal_status};
    use crate::usage::RepositoryProbe;
    use devcoordinator2_executor_protocol::{ProofKind, ValidationTier};
    use std::collections::HashSet;
    use std::path::Path;
    use tempfile::tempdir;
    use time::macros::datetime;

    struct NoProbe;
    impl RepositoryProbe for NoProbe {
        fn probe(
            &self,
            _source: &crate::config::CodexUsageSource,
            _repository: &Path,
            _now_ms: u64,
        ) -> Result<(String, u32, u32), String> {
            Err("source_unavailable".into())
        }
    }

    #[test]
    fn progress_uses_plan_order_test_history_and_truthful_missing_tokens() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repository)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let registry = Registry::new(database.clone());
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let registered = registry.register(&repository, uid, gid).unwrap();
        let repository_id = registered.repository_id.clone();
        database.transaction(move |transaction| {
            transaction.execute("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES('v1111111111111111',?1,1,'September','release','planned','2026-09-01T00:00:00Z','uid:1','2026-09-01T00:00:00Z')",[&repository_id])?;
            transaction.execute("INSERT INTO tasks(task_id,repository_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at) VALUES('p1111111111111111',?1,'v1111111111111111',1,0,'Finished outcome','done',NULL,NULL,NULL,NULL,'goal','done',100,0,'2026-09-03T08:00:00Z','uid:1','2026-09-03T10:00:00Z')",[&repository_id])?;
            transaction.execute("INSERT INTO tasks(task_id,repository_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at) VALUES('p2222222222222222',?1,'v1111111111111111',2,1,'Open owner-facing title','open',NULL,'internal-code test_name',NULL,NULL,'improvement','in_progress',NULL,0,'2026-09-03T09:00:00Z','uid:1','2026-09-03T11:00:00Z')",[&repository_id])?;
            for values in [
                ("p1111111111111111","created",None,None,"2026-09-03T08:00:00Z",None),
                ("p1111111111111111","estimate",None,Some("100"),"2026-09-03T08:01:00Z",None),
                ("p1111111111111111","status",Some("in_progress"),Some("done"),"2026-09-03T10:00:00Z",None),
                ("p2222222222222222","created",None,None,"2026-09-03T09:00:00Z",None),
                ("p2222222222222222","status",Some("done"),Some("in_progress"),"2026-09-03T11:00:00Z",Some("internal-code test_name")),
            ] {
                transaction.execute("INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,from_value,to_value,actor,at,note) VALUES(?1,'task',?2,?3,?4,?5,'uid:1',?6,?7)",rusqlite::params![repository_id,values.0,values.1,values.2,values.3,values.4,values.5])?;
            }
            Ok(())
        }).unwrap();
        let store = TestRunStore;
        let prepared = store
            .prepare(
                &repository.canonicalize().unwrap(),
                "t20260903T120000Z-aabbcc",
                uid,
                gid,
            )
            .unwrap();
        let mut summary = initial_summary(
            "t20260903T120000Z-aabbcc",
            "all",
            "2026-09-03T12:00:00Z",
            uid,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        terminal_status(
            &mut summary,
            TestStatus::Passed,
            "2026-09-03T12:01:00Z".into(),
            60.0,
            Some(0),
            None,
        );
        store
            .write_summary(&prepared.current, &summary, uid, gid)
            .unwrap();
        store
            .record_history(&repository, &summary, uid, gid)
            .unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "clients".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: temporary.path().join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        };
        let clock = Arc::new(FixedClock(datetime!(2026-09-04 12:00 UTC)));
        let usage =
            CodexUsage::with_probe(config, database.clone(), clock.clone(), Arc::new(NoProbe));
        let progress = ProgressService::with_clock(database, registry, usage, clock);
        let report = progress
            .repository(ProgressRepositoryParams {
                wait_for_refresh: false,
                repository_id: registered.repository_id.clone(),
                period: ProgressPeriod::Day,
            })
            .unwrap();
        assert_eq!(report.scope.tasks_total, 2);
        assert_eq!(report.scope.tasks_done, 1);
        assert_eq!(report.scope.planned_lines_done, 100);
        assert_eq!(report.comparison.current.tasks_completed, 1);
        assert_eq!(report.comparison.current.tasks_created, 2);
        assert_eq!(report.comparison.current.planned_lines_completed, 100);
        assert_eq!(report.comparison.current.planned_lines_added, 100);
        assert_eq!(report.comparison.current.test_runs, 1);
        assert_eq!(report.comparison.current.test_pass_rate, Some(1.0));
        assert_eq!(report.comparison.current.total_tokens, None);
        assert_eq!(report.coverage.tokens.state, CoverageState::Unavailable);
        assert_eq!(report.coverage.state, CoverageState::Partial);
        assert_eq!(report.forecast.state, "available");
        assert_eq!(report.release_work[0].title, "Open owner-facing title");
        assert!(report.release_work[0].reopened);
        assert!(report.release_work[0].unblock_condition.is_none());
        assert!(report.release_work[0].reopen_note.is_none());
        let compact = progress.repositories().unwrap();
        assert_eq!(compact.repositories[0].open_tasks, 1);
        assert_eq!(
            compact.repositories[0].next_release.as_ref().unwrap().name,
            "September"
        );
    }
}
