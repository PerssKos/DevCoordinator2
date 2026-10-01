//! Bounded consumer of CodexMulti's existing localUsage/summary contract.
//! This module never opens, migrates, or writes the producer's accounting store.
use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_CACHED_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHED: usize = 96;
const FRESH_FOR: Duration = Duration::from_secs(5);
const RETRY_AFTER: Duration = Duration::from_secs(30);
pub(super) const API_BUDGET: Duration = Duration::from_millis(650);

// Enforce the absolute deadline on every underlying I/O, including a peer
// that slowly streams one frame. A socket timeout alone resets on each read.
struct DeadlineStream {
    socket: UnixStream,
    deadline: Instant,
}
impl std::ops::Deref for DeadlineStream {
    type Target = UnixStream;
    fn deref(&self) -> &UnixStream {
        &self.socket
    }
}
impl io::Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        set_deadline(&self.socket, self.deadline)
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?;
        self.socket.read(buffer)
    }
}
impl io::Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        set_deadline(&self.socket, self.deadline)
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?;
        io::Write::write(&mut self.socket, buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.socket)
    }
}

#[derive(Clone, Default)]
pub(super) struct CollectorApi(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    failed_until: HashMap<PathBuf, Instant>,
}
struct Entry {
    report: Arc<Summary>,
    loaded: Instant,
    bytes: usize,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Summary {
    pub report: Report,
    pub generated_at: u64,
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
}
#[derive(Clone, Deserialize)]
pub(super) struct Snapshot {
    #[serde(alias = "sourceWatermark")]
    pub source_watermark: Option<String>,
    #[serde(alias = "generatedAt")]
    pub generated_at: u64,
    pub freshness: String,
    #[serde(alias = "refreshId")]
    pub refresh_id: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Report {
    pub schema_version: u32,
    pub kind: String,
    pub database_schema_version: u64,
    pub taxonomy_version: i64,
    pub scope: Scope,
    pub time_range: Option<TimeRange>,
    pub coverage: Coverage,
    pub counts: Counts,
    pub provider_tokens: Vec<Token>,
    pub provider_tokens_by_activity: Vec<Activity>,
    #[serde(default)]
    pub cost: Option<SourceCost>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SourceCost {
    pub basis: String,
    pub currency: String,
    pub status: String,
    pub processing_tier: String,
    pub estimated_usd_micros: Option<u64>,
    pub input_usd_micros: Option<u64>,
    pub cached_input_usd_micros: Option<u64>,
    pub cache_write_usd_micros: Option<u64>,
    pub output_usd_micros: Option<u64>,
    pub input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub provider_total_tokens: u64,
    pub priced_observations: u64,
    pub unknown_observations: u64,
    pub rate_card_refs: Vec<String>,
}

impl SourceCost {
    fn usage_cost(&self) -> UsageCost {
        UsageCost {
            status: self.status.clone(),
            basis: self.basis.clone(),
            currency: self.currency.clone(),
            processing_tier: self.processing_tier.clone(),
            estimated_usd: self
                .estimated_usd_micros
                .map(|v| format!("{}.{:06}", v / 1_000_000, v % 1_000_000)),
            estimated_usd_micros: self.estimated_usd_micros,
            input_usd_micros: self.input_usd_micros,
            cached_input_usd_micros: self.cached_input_usd_micros,
            cache_write_usd_micros: self.cache_write_usd_micros,
            output_usd_micros: self.output_usd_micros,
            input_tokens: Some(self.input_tokens),
            cached_input_tokens: Some(self.cached_input_tokens),
            cache_write_tokens: Some(self.cache_write_tokens),
            uncached_input_tokens: Some(self.uncached_input_tokens),
            output_tokens: Some(self.output_tokens),
            reasoning_tokens: Some(self.reasoning_tokens),
            unknown_requests: 0,
            unknown_tokens: 0,
            unknown_observations: self.unknown_observations,
            rate_card_ref: self.rate_card_refs.first().cloned(),
            rate_card_refs: self.rate_card_refs.clone(),
            model_requests: self.priced_observations,
            priced_requests: self.priced_observations,
            unavailable_reasons: BTreeMap::new(),
            matched_rate_cards: Vec::new(),
        }
    }
}
#[derive(Clone, Deserialize)]
pub(super) struct Scope {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
}
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TimeRange {
    pub start_ms: u64,
    pub end_ms: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Coverage {
    pub state: String,
    pub has_gaps: bool,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Counts {
    pub operations: u64,
    pub model_requests: u64,
    pub tools: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Token {
    pub category: String,
    pub repository_bucket: String,
    pub measurement_provenance: String,
    pub measured_tokens: u64,
    pub exact_tokens: Option<u64>,
    pub unknown_observations: u64,
    pub observation_count: u64,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Activity {
    pub phase: String,
    pub activity: String,
    pub attribution_provenance: String,
    pub measured_tokens: u64,
    pub exact_tokens: Option<u64>,
    pub unknown_observations: u64,
}

impl CollectorApi {
    pub(super) fn summary(
        &self,
        source: &CodexUsageSource,
        repository: Option<&str>,
        start: u64,
        end: u64,
        deadline: Instant,
    ) -> Result<Arc<Summary>, String> {
        let path = source.api_socket.as_ref().ok_or("api_not_configured")?;
        let key = format!(
            "{}:{path:?}:{}:{start}:{end}",
            source.uid,
            repository.unwrap_or("all")
        );
        let mut state = self.0.try_lock().map_err(|_| "api_busy")?;
        if let Some(entry) = state
            .entries
            .get(&key)
            .filter(|entry| entry.loaded.elapsed() < FRESH_FOR)
        {
            return Ok(entry.report.clone());
        }
        if state
            .failed_until
            .get(path)
            .is_some_and(|until| *until > Instant::now())
        {
            return Err("api_unavailable".into());
        }
        // The lock coalesces source requests; callers never queue unbounded work.
        let result = fetch(source, repository, start, end, deadline);
        match result {
            Ok((report, bytes)) => {
                state.failed_until.remove(path);
                while state.entries.len() >= MAX_CACHED
                    || state.entries.values().map(|e| e.bytes).sum::<usize>() + bytes
                        > MAX_CACHED_BYTES
                {
                    let oldest = state
                        .entries
                        .iter()
                        .min_by_key(|(_, e)| e.loaded)
                        .map(|(k, _)| k.clone());
                    if let Some(oldest) = oldest {
                        state.entries.remove(&oldest);
                    } else {
                        break;
                    }
                }
                let report = Arc::new(report);
                state.entries.insert(
                    key,
                    Entry {
                        report: report.clone(),
                        loaded: Instant::now(),
                        bytes,
                    },
                );
                Ok(report)
            }
            Err(reason) => {
                state
                    .failed_until
                    .insert(path.clone(), Instant::now() + RETRY_AFTER);
                Err(reason)
            }
        }
    }
}

fn fetch(
    source: &CodexUsageSource,
    repository: Option<&str>,
    start: u64,
    end: u64,
    deadline: Instant,
) -> Result<(Summary, usize), String> {
    let path = source.api_socket.as_ref().ok_or("api_not_configured")?;
    let metadata = path.metadata().map_err(|_| "api_unavailable")?;
    if !metadata.file_type().is_socket() || metadata.uid() != source.uid {
        return Err("api_unavailable".into());
    }
    let budget = deadline
        .checked_duration_since(Instant::now())
        .ok_or("query_budget_exhausted")?;
    // Connect has a deadline even when the server's accept queue is full.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| "api_unavailable")?;
    let stream = runtime.block_on(async {
        tokio::time::timeout(budget, tokio::net::UnixStream::connect(path))
            .await
            .map_err(|_| "query_budget_exhausted")?
            .map_err(|_| "api_unavailable")?
            .into_std()
            .map_err(|_| "api_unavailable")
    })?;
    stream
        .set_nonblocking(false)
        .map_err(|_| "api_unavailable")?;
    // Bind the configured same-owner source to the actual peer, including a replaced socket.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    if result != 0 || credentials.uid != source.uid {
        return Err("api_unavailable".into());
    }
    set_deadline(&stream, deadline)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_RESPONSE))
        .max_frame_size(Some(MAX_RESPONSE));
    let (mut socket, _) = tungstenite::client::client_with_config(
        "ws://localhost/rpc",
        DeadlineStream {
            socket: stream,
            deadline,
        },
        Some(config),
    )
    .map_err(|_| "api_unavailable")?;
    let initialize = exchange(
        &mut socket,
        1,
        "initialize",
        json!({"clientInfo":{"name":"devcoordinator2-usage","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
        deadline,
    )?;
    // Do not route a configured source to another account's daemon.
    if initialize.get("codexHome").and_then(Value::as_str) != source.codex_home.to_str() {
        return Err("api_unavailable".into());
    }
    socket
        .send(Message::Text(
            json!({"method":"initialized"}).to_string().into(),
        ))
        .map_err(|_| "api_unavailable")?;
    let value = exchange(
        &mut socket,
        2,
        "localUsage/summary",
        json!({"repositoryKey":repository,"fromAt":start,"toAt":end}),
        deadline,
    )?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| "api_contract_unsupported")?
        .len();
    let summary: Summary = serde_json::from_value(value).map_err(|_| "api_contract_unsupported")?;
    validate(&summary, repository, start, end)?;
    // Dropping the connection cancels any pending source work on failures as well.
    Ok((summary, bytes.saturating_mul(4)))
}
fn set_deadline(stream: &UnixStream, deadline: Instant) -> Result<(), String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or("query_budget_exhausted")?;
    stream
        .set_read_timeout(Some(remaining))
        .and_then(|_| stream.set_write_timeout(Some(remaining)))
        .map_err(|_| "api_unavailable".into())
}
fn exchange(
    socket: &mut WebSocket<DeadlineStream>,
    id: u64,
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<Value, String> {
    set_deadline(socket.get_ref(), deadline)?;
    socket
        .send(Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .map_err(|_| "api_unavailable")?;
    for _ in 0..64 {
        set_deadline(socket.get_ref(), deadline)?;
        match socket.read().map_err(|_| "api_unavailable")? {
            Message::Text(text) => {
                let response: Value =
                    serde_json::from_str(&text).map_err(|_| "api_contract_unsupported")?;
                if response.get("id").and_then(Value::as_u64) == Some(id) {
                    return response
                        .get("result")
                        .cloned()
                        .ok_or_else(|| "api_contract_unsupported".into());
                }
                // Ignore unrelated notifications; never copy raw content or errors into results.
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => return Err("api_contract_unsupported".into()),
        }
    }
    Err("api_contract_unsupported".into())
}
fn validate(
    summary: &Summary,
    repository: Option<&str>,
    start: u64,
    end: u64,
) -> Result<(), String> {
    let r = &summary.report;
    if r.schema_version != 1
        || r.taxonomy_version != i64::from(SUPPORTED_TAXONOMY)
        || r.kind != "usageSummary"
        || r.time_range.as_ref()
            != Some(&TimeRange {
                start_ms: start,
                end_ms: end,
            })
        || r.scope.kind
            != if repository.is_some() {
                "repository"
            } else {
                "all"
            }
        || r.scope.id.as_deref() != repository
        || !matches!(
            r.coverage.state.as_str(),
            "complete" | "partial" | "unknown" | "unobserved" | "unavailable"
        )
    {
        return Err("api_contract_unsupported".into());
    }
    if let Some(snapshot) = &summary.snapshot {
        if !matches!(
            snapshot.freshness.as_str(),
            "fresh" | "stale" | "refreshing" | "failed"
        ) || snapshot
            .source_watermark
            .as_ref()
            .is_some_and(|s| s.len() > 256)
            || snapshot.refresh_id.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err("api_contract_unsupported".into());
        }
    }
    if let Some(cost) = &summary.report.cost {
        let provider_total = summary
            .report
            .provider_tokens
            .iter()
            .filter(|token| token.category == "total_tokens")
            .map(|token| token.measured_tokens)
            .sum::<u64>();
        let components = [
            cost.input_usd_micros,
            cost.cached_input_usd_micros,
            cost.cache_write_usd_micros,
            cost.output_usd_micros,
        ];
        let total = components
            .iter()
            .flatten()
            .try_fold(0u64, |sum, v| sum.checked_add(*v));
        if cost.basis != "api_equivalent"
            || cost.currency != "USD"
            || cost.processing_tier != "standard"
            || !matches!(cost.status.as_str(), "complete" | "partial" | "unavailable")
            || (cost.status == "complete"
                && (components.iter().any(Option::is_none)
                    || total
                        .zip(cost.estimated_usd_micros)
                        .is_none_or(|(parts, total)| total < parts || total - parts > 3)
                    || cost.provider_total_tokens != provider_total
                    || cost.rate_card_refs.is_empty()))
            || cost
                .rate_card_refs
                .iter()
                .any(|r| r.len() > 128 || r.chars().any(char::is_control))
        {
            return Err("api_contract_unsupported".into());
        }
    }
    Ok(())
}

impl Summary {
    pub(super) fn source_report(&self, repository: Option<&str>, buckets: usize) -> SourceReport {
        let r = &self.report;
        let partial = r.coverage.has_gaps || r.coverage.state != "complete";
        let mut result = SourceReport {
            supplied_cost: r.cost.as_ref().map(SourceCost::usage_cost),
            snapshot: Some(devcoordinator2_api::results::UsageSnapshot {
                updated_at_ms: Some(
                    self.snapshot
                        .as_ref()
                        .map_or(self.generated_at, |s| s.generated_at),
                ),
                refreshing: self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.freshness == "refreshing"),
                refresh_failed: self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| matches!(s.freshness.as_str(), "stale" | "failed")),
                progress_completed: None,
                progress_total: None,
                progress_stage: None,
            }),
            database_schema: u32::try_from(r.database_schema_version).unwrap_or(u32::MAX),
            taxonomy_version: u32::try_from(r.taxonomy_version).unwrap_or(u32::MAX),
            freshest_at_ms: Some(
                self.snapshot
                    .as_ref()
                    .map_or(self.generated_at, |s| s.generated_at),
            ),
            phase_series: vec![BTreeMap::new(); buckets],
            token_buckets_observed: vec![false; buckets],
            bucket_coverage: vec![CoverageState::Unobserved; buckets],
            // Aggregated durations cannot be unioned across collectors. Do not invent intervals.
            request_unknown: 1,
            execution_unknown: 1,
            agent_unknown: 1,
            ..Default::default()
        };
        for t in &r.provider_tokens {
            if t.measurement_provenance != "provider_reported"
                || repository.is_some_and(|key| t.repository_bucket != key)
            {
                continue;
            }
            if !TOKEN_CATEGORIES
                .iter()
                .any(|(category, _)| *category == t.category)
            {
                continue;
            }
            result.evidence |= t.observation_count > 0;
            if t.observation_count > 0 {
                *result.tokens.entry(t.category.clone()).or_default() += t.measured_tokens;
            }
            let state = if partial || t.unknown_observations > 0 || t.exact_tokens.is_none() {
                "partial"
            } else {
                "complete"
            };
            *result.token_observations.entry(state.into()).or_default() += t.observation_count;
        }
        {
            result.operation_count = r.counts.operations;
            result.model_request_count = r.counts.model_requests;
            result.tool_count = r.counts.tools;
            for a in &r.provider_tokens_by_activity {
                let key = (safe_phase(&a.phase), safe_label(&a.activity));
                *result.activities.entry(key.clone()).or_default() += a.measured_tokens;
                *result
                    .activity_provenance
                    .entry((key.0, key.1, safe_label(&a.attribution_provenance)))
                    .or_default() += a.measured_tokens;
                if a.exact_tokens.is_none() || a.unknown_observations > 0 {
                    increment(&mut result.coverage_events, "partial");
                }
            }
        }
        if buckets == 1 {
            for ((phase, _), value) in &result.activities {
                *result.phase_series[0].entry(phase.clone()).or_default() += value;
            }
            if let Some(total) = result.tokens.get("total_tokens").copied() {
                let classified = result.phase_series[0]
                    .values()
                    .copied()
                    .try_fold(0u64, u64::checked_add);
                match classified {
                    Some(classified) if classified <= total => {
                        *result.phase_series[0]
                            .entry("unattributed".into())
                            .or_default() += total - classified;
                    }
                    _ => {
                        result.phase_series[0] = BTreeMap::from([("unattributed".into(), total)]);
                        increment(&mut result.coverage_events, "partial");
                    }
                }
            }
            result.token_buckets_observed[0] = result.tokens.contains_key("total_tokens");
            result.bucket_coverage[0] = if partial {
                CoverageState::Partial
            } else {
                CoverageState::Complete
            };
        }
        result
    }
}

impl CodexUsage {
    /// Source-wide repository totals: one exact-window summary per configured
    /// source, independent of the number of registered repositories.
    pub(super) fn api_repositories(
        &self,
        repositories: &[RepositoryRecord],
        range: UsageRange,
    ) -> Result<Option<UsageRepositories>, ProtocolError> {
        if self.config.codex_usage_sources.is_empty()
            || self
                .config
                .codex_usage_sources
                .iter()
                .any(|s| s.api_socket.is_none())
        {
            return Ok(None);
        }
        let now = self.now_ms()?;
        // Display snapshots share one five-second window. Exact review reads
        // use their original boundaries and never enter this display cache.
        let observed = now / 5_000 * 5_000;
        let (start, end, bucket, count) = usage_window(&range, observed);
        let deadline = Instant::now() + API_BUDGET;
        let mut rows = Vec::with_capacity(repositories.len());
        for repository in repositories {
            let mut reports = Vec::new();
            let mut failures = BTreeMap::new();
            for source in &self.config.codex_usage_sources {
                match self.repository_key(source, repository, now, false) {
                    Ok(key) => match self.api.summary(source, Some(&key), start, end, deadline) {
                        Ok(summary) => {
                            reports.push((source.uid, summary.source_report(Some(&key), count)))
                        }
                        Err(_) => return Ok(None),
                    },
                    Err(error) => increment(&mut failures, &error.message),
                }
            }
            let report = combine(
                repository,
                range.clone(),
                observed,
                start,
                bucket,
                count,
                &reports,
                failures,
                self.config.codex_usage_sources.len(),
            );
            rows.push(UsageRepositoryRow {
                repository_id: report.repository_id,
                display_name: report.display_name,
                range: report.range,
                coverage: report.coverage,
                total_tokens: report.totals.total_tokens,
                model_requests: Some(report.totals.model_requests),
                tool_calls: Some(report.totals.tool_calls),
                execution_wall_ms: Some(report.time.execution_wall.measured_ms),
                cost: report.totals.cost,
            });
        }
        Ok(Some(UsageRepositories {
            range,
            generated_at_ms: observed,
            repositories: rows,
        }))
    }
}

/// Never add a producer estimate and a local estimate of the same source.
/// Missing components remain unknown and make the combined valuation partial.
pub(super) fn merge_costs(costs: &[UsageCost], source_gaps: bool) -> UsageCost {
    let mut result = UsageCost {
        basis: "api_equivalent".into(),
        currency: "USD".into(),
        processing_tier: "standard".into(),
        ..Default::default()
    };
    let mut partial = source_gaps;
    let mut refs = BTreeSet::new();
    macro_rules! sum {
        ($field:ident) => {{
            let mut amount = None;
            for cost in costs {
                match cost.$field {
                    Some(v) => {
                        amount = Some(amount.unwrap_or(0u64).saturating_add(v));
                    }
                    None => partial = true,
                }
            }
            result.$field = amount;
        }};
    }
    sum!(estimated_usd_micros);
    sum!(input_usd_micros);
    sum!(cached_input_usd_micros);
    sum!(cache_write_usd_micros);
    sum!(output_usd_micros);
    sum!(input_tokens);
    sum!(cached_input_tokens);
    sum!(cache_write_tokens);
    sum!(uncached_input_tokens);
    sum!(output_tokens);
    // Reasoning is an optional output subset, not a separate billed component.
    result.reasoning_tokens = costs
        .iter()
        .filter_map(|c| c.reasoning_tokens)
        .reduce(u64::saturating_add);
    for cost in costs {
        partial |= cost.status != "complete";
        result.model_requests = result.model_requests.saturating_add(cost.model_requests);
        result.priced_requests = result.priced_requests.saturating_add(cost.priced_requests);
        result.unknown_requests = result
            .unknown_requests
            .saturating_add(cost.unknown_requests);
        result.unknown_tokens = result.unknown_tokens.saturating_add(cost.unknown_tokens);
        result.unknown_observations = result
            .unknown_observations
            .saturating_add(cost.unknown_observations);
        refs.extend(cost.rate_card_refs.clone());
        for card in &cost.matched_rate_cards {
            if !result.matched_rate_cards.contains(card) {
                result.matched_rate_cards.push(card.clone());
            }
        }
        merge_counts(&mut result.unavailable_reasons, &cost.unavailable_reasons);
    }
    if source_gaps {
        increment(&mut result.unavailable_reasons, "unavailable_collectors");
    }
    result.rate_card_refs = refs.into_iter().collect();
    result.rate_card_ref = result.rate_card_refs.first().cloned();
    result.status = if result.estimated_usd_micros.is_none() {
        "unavailable"
    } else if partial {
        "partial"
    } else {
        "complete"
    }
    .into();
    result.estimated_usd = result
        .estimated_usd_micros
        .map(|v| format!("{}.{:06}", v / 1_000_000, v % 1_000_000));
    result
}

#[cfg(test)]
#[path = "usage_api_tests.rs"]
mod tests;
