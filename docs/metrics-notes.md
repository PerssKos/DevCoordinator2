# Health Metrics Notes (Phase 4 — design only)

Owner decision DC2-2026-08-22-METRICS. Implemented in Phase 4
(`rust/control/src/metrics_sampler.rs`, `metrics.rs`, and `alerts.rs`); this note records the
cadence and model.

## Cadence and retention

- CPU and memory sampled every 15 seconds from cgroups (managed units) and
  the Docker stats API (managed containers).
- One-minute aggregates persisted (min/avg/max per series).
- Storage sampled every 5 minutes (directory/volume/data-dir sizes).
- Retention: **30 days** for low-cardinality host, repository, daemon and
  shared trend series. High-cardinality component, container and test series
  retain **7 days** of one-minute aggregates because they are disposable
  runtime observations. Expired rows are deleted directly; no governed test
  evidence or permanent planning history is stored here. The metric store is
  rebuildable and does not contain a second backup or migration of metric
  history.

## Series model (sketch)

One bounded time-series table keyed by
`(subject_kind, subject_id, metric, minute_utc)` with `min/avg/max` REAL
columns. Subject kinds: host, repository, deployment, component, test,
container, postgres, shared/unattributed. Reconciliation invariant:

```
managed repositories + DevCoordinator + shared/unattributed = host total
```

Never invent per-database CPU/memory for shared PostgreSQL; report shared
overhead as shared. Never collect query text, row values, credentials, or
connection strings.

## Alerts

Sustained-threshold alerts with active-state deduplication and a single
recovery message; current alerts live in the authority DB (`database-ledger.md`).
