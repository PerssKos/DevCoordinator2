use super::review_aggregate::Groups;
use super::*;
use devcoordinator2_api::review::ReviewUsage;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Projection {
    Full,
    Tokens,
}
impl CodexUsage {
    pub(super) fn review_measurements(
        &self,
        repository: &RepositoryRecord,
        workstream: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        deadline: Instant,
    ) -> Result<ReviewUsage, ProtocolError> {
        self.measurements(
            repository,
            workstream,
            start_ms,
            end_ms,
            deadline,
            Projection::Full,
        )
    }

    pub(super) fn measurements(
        &self,
        repository: &RepositoryRecord,
        workstream: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        deadline: Instant,
        projection: Projection,
    ) -> Result<ReviewUsage, ProtocolError> {
        let now_ms = self.now_ms()?;
        let rates = self.rate_cards()?;
        // Independent collectors share the same deadline, not one another's time.
        let results = thread::scope(|scope| {
            let workers = self
                .config
                .codex_usage_sources
                .iter()
                .map(|source| {
                    let rates = &rates.cards;
                    scope.spawn(move || {
                        let result = self.review_source(
                            source, repository, workstream, now_ms, start_ms, end_ms, deadline,
                            projection, rates,
                        );
                        (source.uid, result)
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().map_err(|_| "source_unavailable".to_string()))
                .collect::<Vec<_>>()
        });
        let mut reports = Vec::new();
        let mut failures = BTreeMap::new();
        let mut loading = None;
        let mut groups = Groups::default();
        for (index, result) in results.into_iter().enumerate() {
            match result {
                Ok((uid, Ok((report, source_groups)))) => {
                    groups.merge(source_groups, index).map_err(source_error)?;
                    reports.push((uid, report));
                }
                Ok((_, Err(reason))) | Err(reason) => {
                    if super::collector_api::is_pending(&reason)
                        && let Some(source) = self.config.codex_usage_sources.get(index)
                        && let Ok(key) = self.repository_key(source, repository, now_ms, false)
                        && let Ok(contexts) = crate::usage_context::read(
                            &self.authority,
                            source.uid,
                            &repository.repository_id,
                            start_ms,
                            end_ms,
                        )
                    {
                        let candidate = self.api.loading_for(
                            source,
                            Some(&key),
                            None,
                            true,
                            workstream,
                            &contexts.bindings,
                            start_ms,
                            end_ms,
                        );
                        loading = match (loading.take(), candidate) {
                            (None, candidate) => candidate,
                            (Some(existing), Some(candidate)) => {
                                Some(devcoordinator2_api::results::UsageLoading {
                                    state: if existing.state == candidate.state {
                                        existing.state
                                    } else {
                                        "reading".into()
                                    },
                                    elapsed_ms: existing.elapsed_ms.max(candidate.elapsed_ms),
                                    estimated_remaining_ms: None,
                                    estimate_samples: 0,
                                    estimate_basis: None,
                                })
                            }
                            (existing, None) => existing,
                        };
                    }
                    increment(&mut failures, &reason)
                }
            }
        }
        let outcomes = groups
            .finish(!failures.is_empty() || self.config.codex_usage_sources.is_empty())
            .map_err(source_error)?;
        let report = combine(
            repository,
            UsageRange::Hours24,
            now_ms,
            start_ms,
            end_ms - start_ms,
            1,
            &reports,
            failures,
            loading,
            self.config.codex_usage_sources.len(),
        );
        let usage = ReviewUsage {
            coverage: report.coverage,
            totals: report.totals,
            activities: report.activities,
            time: report.time,
            tools: report.tools,
            semantics: report.semantics,
            outcomes,
        };
        Ok(usage)
    }

    #[allow(clippy::too_many_arguments)]
    fn review_source(
        &self,
        source: &CodexUsageSource,
        repository: &RepositoryRecord,
        workstream: Option<&str>,
        now_ms: u64,
        start_ms: u64,
        end_ms: u64,
        deadline: Instant,
        projection: Projection,
        rates: &[devcoordinator2_api::rate_card::RateCard],
    ) -> Result<(SourceReport, Groups), String> {
        if super::has_usage_v2_manifest(source) {
            let key = self
                .repository_key_until(source, repository, now_ms, true, deadline)
                .map_err(|error| error.message)?;
            let contexts = crate::usage_context::read(
                &self.authority,
                source.uid,
                &repository.repository_id,
                start_ms,
                end_ms,
            )?;
            let summary = self.api.summary_with_workstream(
                source,
                Some(&key),
                None,
                true,
                workstream,
                &contexts.bindings,
                start_ms,
                end_ms,
                deadline.min(Instant::now() + super::collector_api::API_BUDGET),
            )?;
            let mut report = summary.source_report(Some(&key), 1, None, true);
            let mut groups = summary
                .review
                .as_ref()
                .map(|review| super::review_aggregate::from_api(&review.outcomes))
                .ok_or("api_contract_unsupported")?;
            if self.binding_capture_failed(source.uid, &repository.repository_id) {
                groups.gap("work_context_capture_failed", 1);
                increment(&mut report.coverage_events, "partial");
            }
            if contexts.truncated || contexts.rejected > 0 {
                increment(&mut report.coverage_events, "partial");
                if contexts.truncated {
                    groups.gap("work_context_bindings_truncated", 1);
                }
                if contexts.rejected > 0 {
                    groups.gap("work_context_binding_conflict", contexts.rejected);
                }
            }
            let _ = (projection, rates);
            return if Instant::now() >= deadline {
                Err("query_budget_exhausted".into())
            } else {
                Ok((report, groups))
            };
        }
        Err("legacy_source_unsupported".into())
    }
}
