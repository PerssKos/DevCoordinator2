# Codex usage consumer

DevCoordinator reads the producer's versioned `usage-v2` API. CodexMulti owns
the append-only journal, immutable segments, retention, publication and token
observations. Coordinator does not open, migrate or fall back to
`usage/usage.sqlite3`, and does not probe `codex usage`.

## Local contract

The private source policy selects a same-owner Unix socket. The consumer checks
its owner, the connected peer UID and the `codexHome` returned by `initialize`.
It requires the advertised usage-v2 capability and supported contract version
before calling `localUsage/summary`. A successful transport alone does not make
a collector available: the response must also have the negotiated version,
format, scope, exact UTC window and consistent snapshot metadata.

`localUsage/repositoryIdentity` resolves the producer's opaque repository key
for a registered local checkout. The path travels only as private IPC input;
it is not retained in normal usage records or returned in reports. Existing
repository mappings may be refreshed through this API. A retired source or an
unsupported capability remains a separately identified unavailable collector.

The summary keeps the existing `generatedAt`, `report` and `snapshot` envelope.
Its additive fields identify contract version 1 and source `usage-v2`. The
manifest format is 3; `databaseSchemaVersion` retains its historical JSON name
for compatibility. Snapshot generation, published watermark, represented
journal watermark, publication time, freshness and pending work describe the
same immutable view. The journal watermark may exceed the published watermark
when the reader includes a complete bounded tail.

Global, account, repository and thread queries use explicit producer filters.
A global total cannot establish a repository total. Repository totals cannot
establish task or workstream attribution. A continuation or comparison that
requires a specific snapshot supplies the producer's `expectedSnapshot` value;
an expired snapshot is an explicit failure rather than permission to combine
different generations. Detailed performance reads use finite windows and
bounded pages. Cached snapshots do not authorize a full-history foreground scan.

## Explicit work context

Coordinator retains a content-free declaration when an authorized governed
request supplies valid work context. The authority index is keyed by caller
source and operation ID and includes the Coordinator repository, native project,
thread, turn, workstream, outcome and experiment reference. It stores
declarations, not a duplicate usage event history. Conflicting declarations do
not overwrite the original context or silently acquire attribution.

The read-only producer request accepts a bounded `workContextBindings` array.
Each entry has `schemaVersion`, `operationId`, `capturedAtMs`, and `workContext`.
The context has `schemaVersion`, `id`, `repositoryId`, `nativeProjectId`,
`threadId`, `turnId`, `workstreamId`, `outcomeId`, and `experimentRef`. Optional
fields remain absent when undeclared. `repositoryId` inside the context names
the Coordinator repository; the summary's repository selector names the
producer repository. These are separate identity namespaces.

Only exact operation IDs with matching recorded repository/thread/turn evidence
can contribute attributed rows. Missing, conflicting or mismatched context
remains explicitly unattributed. A binding alone creates no operation, token
observation or delivery receipt. The optional producer-side binding file is
not required for Coordinator ingestion and is never written by the collector.

`includeReview` requests the finite review projection. Workstream selection
applies to outcome effort and rows; the repository summary retains its stated
scope. Unknown-context work remains visible. `review.prepare` preserves its
existing JSON fields, and review acceptance still requires interpreted evidence.
The integration does not change verdict rules to manufacture a completed review.

## Coverage, timing and privacy

Unavailable collectors are excluded, never counted as synthetic zeroes.
Provider total tokens are not reconstructed by adding components. Cached input
and reasoning output are subsets. Unknown measurements retain null exact values.
Summed concurrent operation durations are not elapsed wall time or active-agent
time; dimensions lacking the necessary intervals stay unknown.

Ordinary reports omit credentials, raw arguments, prompts, response bodies,
private paths and contributing account identities. Source references used by
the collector remain internal; human-facing source labels do not expose UIDs or
producer private identifiers. Existing operator/administrator access rules and
same-owner IPC authentication remain unchanged.

Reads retain response byte limits, bounded caches and deadlines. A transport
deadline bounds the consumer's wait; disconnecting does not necessarily stop a
producer worker already executing a bounded query. Failed capability checks,
expired snapshots, incomplete bindings and pending publication remain explicit
coverage states.

A cold exact-window read may take longer than the foreground review budget.
The collector coalesces it in the existing cache, returns explicit refreshing
coverage when the foreground wait expires, and retains the bounded background
result for the next identical request. A foreground timeout is not a thirty-second
source failure. Changing the window or work bindings selects a different cache
entry; a warm result is never borrowed from another scope.
The owned source read has a five-minute deadline while foreground source waits
remain bounded to 650 milliseconds. Performance overview uses this same exact
period and explicit work-context projection for recent and historical periods,
so elapsed time cannot switch an unfinished request to another cache or discard
its outcome rows, activity, or costs. Lifetime requests retain their finite
zero-to-selected-time scope; unbounded producer metadata does not provide a
provider-reported total and cannot substitute for that measurement.

Console writes the selected period and the separate all-recorded cutoff into
its route before the first request. Reloading therefore consumes the same
pending or completed queries instead of starting new scans with new timestamps.
The Refresh control advances relative windows and the all-recorded cutoff;
custom period boundaries remain unchanged. Pending overview and review
measurements use loading text until their bounded source read completes, rather
than claiming that data is unavailable or that the period has no outcomes.

Compact review usage pages retain their 16 KiB evidence-packet budget. Dedicated
Performance usage pages retain measured activity, outcome, kind, and coverage
breakdowns within a 64 KiB page budget; a page that cannot fit fails explicitly.
Complete Performance results are bounded to 128 KiB, leaving room for the
transport envelope. A repository-wide Performance chart may aggregate canonical
repository activity rows when the producer omits that aggregate map. Explicit
producer maps remain intact, and repository rows never allocate activity to a
named workstream or outcome.

The producer scopes outcome effort by workstream, while its report totals and
activity rows remain repository-wide. For a named-workstream review, Console
uses the outcome aggregate's own cost and token components and omits the
unrelated repository activity table. Activity/kind chart buckets contain token
measurements only; their detail popovers never substitute aggregate components.
Missing component detail remains unavailable, and each outcome retains its own
measured cost.

The five-second freshness period starts revalidation; it does not make completed
measurements unavailable. A later request returns the last valid exact-query
snapshot while one bounded source refresh runs, including when the caller has
not yet consumed that completed snapshot. Coverage events identify
`api_snapshot_stale` and the pending or failed refresh reason. The display
snapshot's `updated_at_ms` is the producer's summary-generation time, so a new
request cannot make old measurements appear newly observed. The separate source
snapshot retains the producer's original publication times and frontiers.

Usage chart retries also retain the pending display entry's exact source
cutoff. A moving wall clock within the same chart bucket cannot abandon a
completed source query for a newer window. After that display cycle completes,
normal revalidation may advance the cutoff. The existing projection, bucket,
selection, rate-card, queue, and memory boundaries remain unchanged.
The retained cutoff does not replace the report's assembly time or the
producer's original snapshot generation and publication timestamps.

If an uncached query waits behind another read of the same source, its normal
retry gets a turn before a completed query starts another stale revalidation.
The completed query still returns its own measured snapshot; no result is
borrowed across time windows, repositories, workstreams, or binding revisions.
This uses only bounded admission hints inside the existing cache, not queued
query payloads or additional workers. Hints expire after fifteen seconds
without a retry and share the existing admission-memory budget. Other sources
remain independent.

A failed refresh preserves these measurements with
`refresh_failed: true` and the precise bounded reason until a successful refresh
replaces them. Cold failures still have no measurements. Existing memory, byte,
admission, retry and eviction bounds apply; no usage copy is stored durably.

Stale source publication remains a coverage gap. Pending journal entries may
keep its snapshot refreshing, but a successful stale read is not a failed
refresh. Only a failed source state or an actual refresh error marks failure.

While an owned source query is running, `coverage.loading` may report
`waiting_for_source`, `reading`, or `estimate_exceeded`, with elapsed time and
an optional `estimated_remaining_ms`. The estimate is present only after three
recent successful reads with the same source, repository, selection, binding,
workstream, and window class; it uses a bounded median and is labeled with
`recent_comparable_source_reads`. A first read, a queued read, or an unrelated
scope keeps the estimate unknown. The five-minute source deadline is a timeout
boundary, never an ETA. Loading clears when the owned read completes, even when
the producer reports stale publication or partial coverage.

## Verification and activation

Acceptance exercises authorized context ingestion and `review.prepare` through
the real collector transport. The producer fixture uses a v3 manifest, journal
and Parquet segments, including late facts and explicit bindings. Regressions
cover available fresh sources beside unsupported legacy sources, unchanged
review JSON, non-empty attributed rows, unattributed remainder, negotiation,
snapshot consistency and refusal to open retired SQLite.

Coordinator self-validation runs through its source executor's `run-local`
surface. Codex producer checks use their governed repository graph. Live
activation uses the reviewed root installer only after candidate acceptance;
source commits or parsed fixture responses alone are not live-deployment proof.
