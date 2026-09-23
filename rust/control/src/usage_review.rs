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
        // Independent collectors share the same deadline, not one another's time.
        let results = thread::scope(|scope| {
            let workers = self
                .config
                .codex_usage_sources
                .iter()
                .map(|source| {
                    scope.spawn(move || {
                        let result = self.review_source(
                            source, repository, workstream, now_ms, start_ms, end_ms, deadline,
                            projection,
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
        let mut groups = Groups::default();
        for (index, result) in results.into_iter().enumerate() {
            match result {
                Ok((uid, Ok((report, source_groups)))) => {
                    groups.merge(source_groups, index).map_err(source_error)?;
                    reports.push((uid, report));
                }
                Ok((_, Err(reason))) | Err(reason) => increment(&mut failures, &reason),
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
            self.config.codex_usage_sources.len(),
        );
        Ok(ReviewUsage {
            coverage: report.coverage,
            totals: report.totals,
            activities: report.activities,
            time: report.time,
            tools: report.tools,
            semantics: report.semantics,
            outcomes,
        })
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
    ) -> Result<(SourceReport, Groups), String> {
        for attempt in 0..2 {
            if Instant::now() >= deadline {
                return Err("query_budget_exhausted".into());
            }
            let key = self
                // A former identity may still exist; existence alone cannot validate a link.
                // The identity-only probe is bounded by this review's shared deadline.
                .repository_key_until(source, repository, now_ms, true, deadline)
                .map_err(|error| error.message)?;
            let connection = open_source_until(source, deadline)?;
            connection
                .execute_batch("BEGIN")
                .map_err(|_| "source_unavailable")?;
            let schema = maximum_version(&connection, "_sqlx_migrations")?;
            let taxonomy = maximum_version(&connection, "taxonomy_versions")?;
            if !SUPPORTED_DATABASE_SCHEMAS.contains(&schema) || taxonomy != SUPPORTED_TAXONOMY {
                return Err("schema_unsupported".into());
            }
            let canonical = canonical_repository(&connection, &key)?;
            let family = repository_family(&connection, &canonical)?;
            if !repository_exists(&connection, &family)? {
                if attempt == 0 {
                    self.delete_link(source.uid, &repository.repository_id)
                        .map_err(|error| error.message)?;
                    continue;
                }
                return Err("mapping_unavailable".into());
            }
            if schema >= 7
                && projection == Projection::Tokens
                && let Some(facts) =
                    super::performance::read(&connection, &family, start_ms, end_ms)?
            {
                let result = super::review_aggregate::display(
                    &connection,
                    facts,
                    workstream,
                    start_ms,
                    end_ms,
                );
                return if Instant::now() >= deadline {
                    Err("query_budget_exhausted".into())
                } else {
                    result
                };
            }
            let result = if schema >= 7 {
                super::review_facts::read(&connection, &family, start_ms, end_ms).and_then(
                    |facts| {
                        super::review_aggregate::aggregate(
                            &connection,
                            facts,
                            workstream,
                            start_ms,
                            end_ms,
                        )
                    },
                )
            } else {
                source_report(
                    &connection,
                    &family,
                    schema,
                    taxonomy,
                    start_ms,
                    end_ms,
                    end_ms - start_ms,
                    1,
                )
                .map(|report| {
                    let groups = Groups::legacy(&report);
                    (report, groups)
                })
            };
            return if Instant::now() >= deadline {
                Err("query_budget_exhausted".into())
            } else {
                result
            };
        }
        Err("mapping_unavailable".into())
    }
}
