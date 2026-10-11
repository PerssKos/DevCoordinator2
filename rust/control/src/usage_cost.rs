//! API-equivalent valuation of canonical provider observations, before aggregation.
use super::*;
use devcoordinator2_api::rate_card::RateCard;

#[derive(Clone, Default)]
pub(super) struct RequestTokens {
    pub fields: BTreeMap<String, Option<u64>>,
    pub incomplete: bool,
    pub at: u64,
}
impl RequestTokens {
    pub fn observe(&mut self, category: &str, value: Option<u64>, incomplete: bool, at: u64) {
        let slot = self.fields.entry(category.to_owned()).or_insert(value);
        if *slot != value {
            *slot = None;
            self.incomplete = true;
        }
        self.incomplete |= incomplete || value.is_none();
        self.at = self.at.max(at);
    }
    fn value(&self, category: &str) -> Option<u64> {
        self.fields.get(category).copied().flatten()
    }
}

#[derive(Clone, Default)]
pub(super) struct CostModel {
    // Numerators retain sub-micro precision until the final projection.
    pub(super) amounts: [u128; 4],
    pub(super) known_components: [bool; 4],
    pub(super) token_components: [u64; 6],
    pub(super) known_token_components: [bool; 6],
    pub(super) cards: BTreeMap<String, RateCard>,
    pub tokens: u64,
    pub requests: u64,
    pub(super) unknown_requests: u64,
    pub(super) unknown_tokens: u64,
    pub(super) reasons: BTreeMap<String, u64>,
}
#[derive(Clone, Default)]
pub(super) struct CostBuckets {
    pub models: BTreeMap<String, CostModel>,
    pub tokens: u64,
    pub operations: u64,
    pub source_gaps: u64,
}

pub(super) fn merge_cost_bucket(target: &mut CostBuckets, source: &CostBuckets) {
    target.tokens = target.tokens.saturating_add(source.tokens);
    target.operations = target.operations.saturating_add(source.operations);
    target.source_gaps += source.source_gaps;
    for (key, value) in &source.models {
        let dst = target.models.entry(key.clone()).or_default();
        for i in 0..4 {
            dst.amounts[i] += value.amounts[i];
            dst.known_components[i] |= value.known_components[i];
        }
        for i in 0..6 {
            dst.token_components[i] =
                dst.token_components[i].saturating_add(value.token_components[i]);
            dst.known_token_components[i] |= value.known_token_components[i];
        }
        dst.cards.extend(value.cards.clone());
        dst.tokens += value.tokens;
        dst.requests += value.requests;
        dst.unknown_requests += value.unknown_requests;
        dst.unknown_tokens += value.unknown_tokens;
        merge_counts(&mut dst.reasons, &value.reasons);
    }
}
fn model_matches(pattern: &str, model: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == model, |prefix| model.starts_with(prefix))
}
pub(super) fn select_card<'a>(
    cards: &'a [RateCard],
    provider: &str,
    model: &str,
    input: u64,
    at: u64,
) -> Option<&'a RateCard> {
    let tier = if input > 272_000 { "long" } else { "short" };
    // A later version can retire a match without reviving the previous version.
    let mut latest = BTreeMap::<&str, &RateCard>::new();
    for card in cards.iter().filter(|c| {
        c.provider == provider
            && c.processing_tier == "standard"
            && c.context_tier == tier
            && model_matches(&c.model_pattern, model)
            && c.effective_from_ms <= at
            && c.effective_to_ms.is_none_or(|end| at < end)
    }) {
        let old = latest.entry(&card.card_id).or_insert(card);
        if (card.effective_from_ms, card.version) > (old.effective_from_ms, old.version) {
            *old = card;
        }
    }
    let mut ranked = latest.values().collect::<Vec<_>>();
    ranked.sort_by_key(|c| {
        (
            c.model_pattern.trim_end_matches('*').len(),
            c.effective_from_ms,
        )
    });
    let chosen = *ranked.last()?;
    if ranked.iter().rev().skip(1).any(|c| {
        c.model_pattern.trim_end_matches('*').len()
            == chosen.model_pattern.trim_end_matches('*').len()
            && c.effective_from_ms == chosen.effective_from_ms
    }) {
        return None;
    }
    chosen.active.then_some(chosen)
}

pub(super) fn request_cost(
    operation: &Operation,
    request: &RequestTokens,
    cards: &[RateCard],
) -> CostBuckets {
    let mut row = CostModel {
        tokens: request.value("total_tokens").unwrap_or(0),
        requests: 1,
        ..Default::default()
    };
    let input = request.value("input_tokens");
    let cached = request.value("input_tokens_details.cached_tokens");
    let written = request.value("input_tokens_details.cache_write_tokens");
    let output = request.value("output_tokens");
    let model = operation.model.as_deref().unwrap_or("unknown");
    let provider = operation.provider_kind.as_deref().unwrap_or("unknown");
    let mut reason = None;
    if let Some(card) = input.and_then(|i| select_card(cards, provider, model, i, request.at)) {
        row.cards
            .insert(format!("{}@{}", card.card_id, card.version), card.clone());
        let conflict = input
            .zip(cached.zip(written))
            .is_some_and(|(i, (c, w))| c.checked_add(w).is_none_or(|sum| sum > i))
            || output
                .zip(request.value("output_tokens_details.reasoning_tokens"))
                .is_some_and(|(o, r)| r > o)
            || input
                .zip(output)
                .zip(request.value("total_tokens"))
                .is_some_and(|((i, o), t)| i.checked_add(o) != Some(t));
        if conflict {
            reason = Some("conflicting_components");
        } else {
            let rates = [
                card.input_usd_micros_per_million,
                card.cached_input_usd_micros_per_million,
                card.cache_write_usd_micros_per_million,
                card.output_usd_micros_per_million,
            ];
            let amounts = [
                input.zip(cached.zip(written)).map(|(i, (c, w))| i - c - w),
                cached,
                written,
                output,
            ];
            let token_components = [
                input,
                cached,
                written,
                input.zip(cached.zip(written)).map(|(i, (c, w))| i - c - w),
                output,
                request.value("output_tokens_details.reasoning_tokens"),
            ];
            for i in 0..4 {
                if let Some(count) = amounts[i] {
                    row.amounts[i] = u128::from(count) * u128::from(rates[i]);
                    row.known_components[i] = true;
                }
            }
            for (i, count) in token_components.into_iter().enumerate() {
                if let Some(count) = count {
                    row.token_components[i] = count;
                    row.known_token_components[i] = true;
                }
            }
            if amounts.iter().any(Option::is_none) {
                reason = Some("missing_token_components");
            }
        }
    } else {
        reason = Some(if input.is_none() {
            "unknown_context_tier"
        } else {
            "rate_not_covered"
        });
    }
    if request.incomplete {
        reason = Some("incomplete_observations");
    }
    if request.value("total_tokens").is_none() {
        reason = Some("missing_provider_total");
    }
    if let Some(reason) = reason {
        row.unknown_requests = 1;
        row.unknown_tokens = row.tokens;
        row.reasons.insert(reason.into(), 1);
    }
    CostBuckets {
        tokens: row.tokens,
        operations: 1,
        models: BTreeMap::from([(format!("{provider}/{model}"), row)]),
        source_gaps: 0,
    }
}

/// Price an indexed dimension aggregate. The producer's hourly dimension
/// rollup preserves model/provider and token components, but it deliberately
/// does not retain one row per request. Use the short-context card as the
/// conservative default for the aggregate and mark the result partial when
/// request-level context or component coverage cannot be recovered.
pub(super) fn aggregate_cost(
    provider: &str,
    model: &str,
    values: &BTreeMap<String, u64>,
    observations: u64,
    incomplete: bool,
    at: u64,
    cards: &[RateCard],
) -> CostBuckets {
    let input = values.get("input_tokens").copied();
    let cached = values.get("input_tokens_details.cached_tokens").copied();
    let written = values
        .get("input_tokens_details.cache_write_tokens")
        .copied();
    let output = values.get("output_tokens").copied();
    let total = values.get("total_tokens").copied().unwrap_or(0);
    let mut row = CostModel {
        tokens: total,
        requests: observations,
        ..Default::default()
    };
    let mut reason = None;
    if let Some(card) = select_card(cards, provider, model, 0, at) {
        row.cards
            .insert(format!("{}@{}", card.card_id, card.version), card.clone());
        let valid = input
            .zip(cached.zip(written))
            .is_some_and(|(i, (c, w))| c.checked_add(w).is_some_and(|sum| sum <= i))
            && output.is_some_and(|o| {
                values
                    .get("output_tokens_details.reasoning_tokens")
                    .is_none_or(|reasoning| *reasoning <= o)
            })
            && input
                .zip(output)
                .zip(values.get("total_tokens"))
                .is_some_and(|((i, o), t)| i.checked_add(o) == Some(*t));
        if valid {
            let input = input.expect("validated input");
            let cached = cached.expect("validated cached input");
            let written = written.expect("validated cache write");
            let output = output.expect("validated output");
            let uncached = input - cached - written;
            let rates = [
                card.input_usd_micros_per_million,
                card.cached_input_usd_micros_per_million,
                card.cache_write_usd_micros_per_million,
                card.output_usd_micros_per_million,
            ];
            for (index, count) in [uncached, cached, written, output].into_iter().enumerate() {
                row.amounts[index] = u128::from(count) * u128::from(rates[index]);
                row.known_components[index] = true;
            }
            for (index, count) in [
                input,
                cached,
                written,
                uncached,
                output,
                values
                    .get("output_tokens_details.reasoning_tokens")
                    .copied()
                    .unwrap_or(0),
            ]
            .into_iter()
            .enumerate()
            {
                row.token_components[index] = count;
                row.known_token_components[index] = true;
            }
        } else {
            reason = Some("missing_token_components");
        }
    } else {
        reason = Some(if model.is_empty() {
            "missing_model"
        } else {
            "rate_not_covered"
        });
    }
    if incomplete {
        reason = Some("aggregated_context_or_incomplete_observations");
    }
    if values.get("total_tokens").is_none() {
        reason = Some("missing_provider_total");
    }
    if let Some(reason) = reason {
        row.unknown_requests = observations;
        row.unknown_tokens = total;
        row.reasons.insert(reason.into(), observations);
    }
    CostBuckets {
        models: BTreeMap::from([(format!("{provider}/{model}"), row)]),
        tokens: total,
        operations: observations,
        ..Default::default()
    }
}

pub(super) fn cost_from_buckets(buckets: &CostBuckets) -> UsageCost {
    let mut amounts = [0u128; 4];
    let mut known = [false; 4];
    let mut token_components = [0_u64; 6];
    let mut known_token_components = [false; 6];
    let mut cards = BTreeMap::new();
    let mut unknown_requests = 0;
    let mut unknown_tokens = 0;
    let mut reasons = BTreeMap::new();
    for row in buckets.models.values() {
        for i in 0..4 {
            amounts[i] += row.amounts[i];
            known[i] |= row.known_components[i];
        }
        for i in 0..6 {
            token_components[i] = token_components[i].saturating_add(row.token_components[i]);
            known_token_components[i] |= row.known_token_components[i];
        }
        cards.extend(row.cards.clone());
        unknown_requests += row.unknown_requests;
        unknown_tokens += row.unknown_tokens;
        merge_counts(&mut reasons, &row.reasons);
    }
    if buckets.source_gaps > 0 {
        reasons.insert("unavailable_collectors".into(), buckets.source_gaps);
    }
    let any = known.iter().any(|v| *v);
    let status = if !any {
        "unavailable"
    } else if unknown_requests > 0 || buckets.source_gaps > 0 {
        "partial"
    } else {
        "complete"
    };
    let micros = |value: u128| u64::try_from(value / 1_000_000).ok();
    let total = any.then(|| micros(amounts.iter().sum())).flatten();
    let refs = cards.keys().cloned().collect::<Vec<_>>();
    UsageCost {
        status: status.into(),
        basis: "api_equivalent".into(),
        currency: "USD".into(),
        processing_tier: "standard".into(),
        estimated_usd: total.map(|v| format!("{}.{:06}", v / 1_000_000, v % 1_000_000)),
        estimated_usd_micros: total,
        input_usd_micros: known[0].then(|| micros(amounts[0])).flatten(),
        cached_input_usd_micros: known[1].then(|| micros(amounts[1])).flatten(),
        cache_write_usd_micros: known[2].then(|| micros(amounts[2])).flatten(),
        output_usd_micros: known[3].then(|| micros(amounts[3])).flatten(),
        input_tokens: known_token_components[0].then_some(token_components[0]),
        cached_input_tokens: known_token_components[1].then_some(token_components[1]),
        cache_write_tokens: known_token_components[2].then_some(token_components[2]),
        uncached_input_tokens: known_token_components[3].then_some(token_components[3]),
        output_tokens: known_token_components[4].then_some(token_components[4]),
        reasoning_tokens: known_token_components[5].then_some(token_components[5]),
        model_requests: buckets.operations,
        priced_requests: buckets.operations.saturating_sub(unknown_requests),
        unknown_requests,
        unknown_tokens,
        unknown_observations: unknown_requests + buckets.source_gaps,
        rate_card_ref: refs.first().cloned(),
        rate_card_refs: refs,
        matched_rate_cards: cards.into_values().collect(),
        unavailable_reasons: reasons,
    }
}

/// Read only content-free model metadata for the already selected owners.
pub(super) fn load_models(
    connection: &Connection,
    operations: &mut BTreeMap<String, review_facts::WorkOperation>,
) -> Result<(), String> {
    let supported:bool = connection.query_row("SELECT COUNT(*)=2 FROM pragma_table_info('model_requests') WHERE name IN ('model','provider_kind')",[],|r|r.get(0)).map_err(|_|"source_unavailable")?;
    if !supported {
        return Ok(());
    }
    let ids = serde_json::to_string(&operations.keys().collect::<Vec<_>>())
        .map_err(|_| "source_unavailable")?;
    let mut stmt=connection.prepare("SELECT operation_id,provider_kind,model FROM model_requests WHERE operation_id IN (SELECT value FROM json_each(?1))").map_err(|_|"source_unavailable")?;
    let rows = stmt
        .query_map([ids], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| "source_unavailable")?;
    for row in rows {
        let (id, provider, model) = row.map_err(|_| "source_unavailable")?;
        if let Some(op) = operations.get_mut(&id) {
            op.operation.provider_kind = provider;
            op.operation.model = model;
        }
    }
    Ok(())
}
