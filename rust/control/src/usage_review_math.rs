use super::{subtract, union_ms};
use devcoordinator2_api::outcomes::OutcomeEffort;
use devcoordinator2_api::outcomes::OutcomeMeasurement;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(super) struct Effort {
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
}

impl Effort {
    pub(super) fn merge(&mut self, other: Self, source: usize) -> Result<(), String> {
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
                .map(|(key, (tokens, unknown))| (key, measurement(tokens, unknown)))
                .collect(),
            operations: self.operations,
            retry_operations: self.retries,
            rework_operations: self.rework,
            provider_total_tokens: measurement(self.tokens, self.unknown_tokens),
            active_agent_ms: measurement(active, self.unknown_active),
            elapsed_execution_ms: measurement(union_ms(&self.elapsed), self.unknown_elapsed),
            recorded_wait_ms: measurement(union_ms(&self.waits), self.unknown_waits),
        })
    }
}

fn measurement(measured: u64, unknown: u64) -> OutcomeMeasurement {
    OutcomeMeasurement {
        measured,
        exact: (unknown == 0).then_some(measured),
        unknown,
    }
}
