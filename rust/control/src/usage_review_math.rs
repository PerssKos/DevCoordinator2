#[cfg(test)]
use super::subtract;
use super::union_ms;
use devcoordinator2_api::outcomes::OutcomeEffort;
use devcoordinator2_api::outcomes::OutcomeMeasurement;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub(super) struct Effort {
    pub(super) supplied_cost: Option<devcoordinator2_api::results::UsageCost>,
    pub(super) aggregate_sources: u64,
    pub(super) inexact_tokens: bool,
    pub(super) inexact_active: bool,
    pub(super) inexact_elapsed: bool,
    pub(super) inexact_waits: bool,
    pub(super) inexact_activities: BTreeSet<String>,
    pub(super) cost: super::CostBuckets,
    pub(super) activities: BTreeMap<String, (u64, u64)>,
    pub(super) operations: u64,
    pub(super) retries: u64,
    pub(super) rework: u64,
    pub(super) tokens: u64,
    pub(super) unknown_tokens: u64,
    pub(super) active: BTreeMap<String, Vec<(u64, u64)>>,
    pub(super) elapsed: Vec<(u64, u64)>,
    pub(super) waits: Vec<(u64, u64)>,
    pub(super) unknown_active: u64,
    pub(super) unknown_elapsed: u64,
    pub(super) unknown_waits: u64,
    /// Pre-aggregated measurements supplied by a fresh producer projection.
    /// They are kept separate from intervals because an aggregate duration is
    /// not an interval and must never be unioned with an invented range.
    pub(super) aggregate_active_ms: u64,
    pub(super) aggregate_elapsed_ms: u64,
    pub(super) aggregate_wait_ms: u64,
}

impl Effort {
    pub(super) fn merge(&mut self, other: Self, source: usize) -> Result<(), String> {
        self.inexact_tokens |= other.inexact_tokens;
        self.inexact_active |= other.inexact_active;
        self.inexact_elapsed |=
            other.inexact_elapsed || (self.aggregate_sources > 0 && other.aggregate_sources > 0);
        self.inexact_waits |=
            other.inexact_waits || (self.aggregate_sources > 0 && other.aggregate_sources > 0);
        self.inexact_activities.extend(other.inexact_activities);
        self.aggregate_sources += other.aggregate_sources;
        if let Some(cost) = other.supplied_cost {
            self.supplied_cost = Some(match self.supplied_cost.take() {
                Some(previous) => super::collector_api::merge_costs(&[previous, cost], false),
                None => cost,
            });
        }
        super::merge_cost_bucket(&mut self.cost, &other.cost);
        for (activity, (tokens, unknown)) in other.activities {
            let value = self.activities.entry(activity).or_default();
            value.0 = value.0.checked_add(tokens).ok_or("usage_overflow")?;
            value.1 += unknown;
        }
        self.operations += other.operations;
        self.retries += other.retries;
        self.rework += other.rework;
        self.tokens = self
            .tokens
            .checked_add(other.tokens)
            .ok_or("usage_overflow")?;
        self.unknown_tokens += other.unknown_tokens;
        self.unknown_active += other.unknown_active;
        self.unknown_elapsed += other.unknown_elapsed;
        self.unknown_waits += other.unknown_waits;
        self.aggregate_active_ms = self
            .aggregate_active_ms
            .saturating_add(other.aggregate_active_ms);
        self.aggregate_elapsed_ms = self
            .aggregate_elapsed_ms
            .checked_add(other.aggregate_elapsed_ms)
            .ok_or("usage_overflow")?;
        self.aggregate_wait_ms = self
            .aggregate_wait_ms
            .checked_add(other.aggregate_wait_ms)
            .ok_or("usage_overflow")?;
        self.elapsed.extend(other.elapsed);
        self.waits.extend(other.waits);
        for (agent, intervals) in other.active {
            self.active
                .entry(format!("{source}:{agent}"))
                .or_default()
                .extend(intervals);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn add_time(
        &mut self,
        operation: &super::review_facts::WorkOperation,
        waits: &[(u64, u64)],
        unknown_waits: u64,
    ) -> Result<(), String> {
        self.operations += 1;
        self.retries += u64::from(operation.retry);
        self.rework += u64::from(operation.rework);
        let is_wait = matches!(
            operation.state.as_str(),
            "user_wait" | "external_wait" | "blocked_wait"
        );
        if !operation.overlaps_window {
            return Ok(());
        }
        self.waits.extend_from_slice(waits);
        self.unknown_waits += unknown_waits;
        let Some(interval) = operation.interval else {
            self.unknown_elapsed += 1;
            if is_wait {
                self.unknown_waits += 1;
            } else {
                self.unknown_active += 1;
            }
            return Ok(());
        };
        self.elapsed.push(interval);
        if is_wait {
            self.waits.push(interval);
        } else if let Some(agent) = &operation.agent_id {
            if unknown_waits > 0 {
                self.unknown_active += 1;
            } else {
                self.active
                    .entry(agent.clone())
                    .or_default()
                    .extend(subtract(interval, waits));
            }
        } else {
            self.unknown_active += 1;
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<OutcomeEffort, String> {
        let active = self.active.values().try_fold(0_u64, |sum, intervals| {
            sum.checked_add(union_ms(intervals))
                .ok_or("usage_overflow".to_string())
        })?;
        Ok(OutcomeEffort {
            activities: self
                .activities
                .into_iter()
                .map(|(key, (tokens, unknown))| {
                    let exact = !self.inexact_activities.contains(&key);
                    (key, measurement(tokens, unknown, exact))
                })
                .collect(),
            operations: self.operations,
            retry_operations: self.retries,
            rework_operations: self.rework,
            provider_total_tokens: measurement(
                self.tokens,
                self.unknown_tokens,
                !self.inexact_tokens,
            ),
            active_agent_ms: measurement(
                active.saturating_add(self.aggregate_active_ms),
                self.unknown_active,
                !self.inexact_active,
            ),
            elapsed_execution_ms: measurement(
                union_ms(&self.elapsed)
                    .checked_add(self.aggregate_elapsed_ms)
                    .ok_or("usage_overflow")?,
                self.unknown_elapsed,
                !self.inexact_elapsed,
            ),
            recorded_wait_ms: measurement(
                union_ms(&self.waits)
                    .checked_add(self.aggregate_wait_ms)
                    .ok_or("usage_overflow")?,
                self.unknown_waits,
                !self.inexact_waits,
            ),
            cost: if let Some(cost) = self.supplied_cost {
                cost
            } else if self.cost.models.is_empty() {
                Default::default()
            } else {
                super::cost_from_buckets(&self.cost)
            },
        })
    }
}

fn measurement(measured: u64, unknown: u64, exact: bool) -> OutcomeMeasurement {
    OutcomeMeasurement {
        measured,
        exact: (unknown == 0 && exact).then_some(measured),
        unknown,
    }
}
