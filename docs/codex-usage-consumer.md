# Codex usage consumer

DevCoordinator consumes CodexMulti's existing `localUsage/summary` operation.
CodexMulti continues to own collection, raw facts, rollups, retention and their
refresh workers. Coordinator keeps only repository identity links in its database
and disposable, bounded display snapshots in memory.

An approved source can add `api_socket` to its private source policy. The socket
is a local WebSocket endpoint using the existing `/rpc` handshake, followed by
`initialize` with `experimentalApi: true`, `initialized`, and
`localUsage/summary`. The connection verifies the configured source UID and
`codexHome`. There is no TCP endpoint, credential import, or new source process.
Omitting the socket retains the previous read-only SQLite behavior. An unavailable
or incompatible API falls back to that same reader; it never rewrites a collector.

The consumer validates report schema 1, taxonomy 1, scope and the echoed half-open
`[fromAt,toAt)` window. Producer database versions are not a wire compatibility
gate. The SQLite fallback retains its separate, explicitly reviewed schema gate.
The source-wide collection requests one all-scope summary per configured source,
then projects its repository token buckets using saved opaque repository links.
Unmapped repositories remain unavailable. Source-wide request/tool counts and
elapsed time cannot be allocated to repositories from the current contract; these
collection cells are null, never inferred zeroes. Opening a repository uses its
scoped report. No account, raw source payload, socket path or collector identifier
is returned to the Console.

Optional snapshot metadata follows the handover contract: `source_watermark`,
`generated_at`, `freshness` (`fresh`, `stale`, `refreshing`, `failed`) and
`refresh_id`. Camel-case aliases are accepted. Observation time is distinct from
the measurement window. Stale or failed source snapshots keep their actual date.
A response may also supply a `cost` object using the existing Coordinator cost
fields: API-equivalent USD, Standard processing basis, fixed integer USD micros,
component/token values, status, unknown counts and versioned rate-card references.
A source estimate takes precedence for that source. It is never added to a local
estimate of the same facts. Without source pricing, the existing cost-capable
SQLite enrichment uses the Coordinator rate-card catalog. Missing rate/model or
component coverage stays partial or unavailable.

The API has a 650 ms shared read budget, a 2 MiB message limit, a 16 MiB source
snapshot budget, at most 96 cached queries, and a 30-second failure backoff. Display
collection queries share five-second observation windows. Exact review evidence
continues to use its original canonical reader until producer contract parity is
proved. Performance snapshot keys include the exact end boundary.

The display cache admits one refresh at a time, queues at most 32 distinct reads,
coalesces duplicate keys, and retains the last usable result on failure. Waiting
for a refresh returns within 650 ms. It has at most 128 reports, a conservative
64 MiB allocation budget and an 8 MiB limit per report. SQLite fallback connections
use a 4 MiB page cache, disable memory mapping and permit temporary joins to spill
to disk. OS filesystem cache is reported separately from anonymous process memory.

## Producer acceptance still required

The current CodexMulti implementation does not yet provide the planned compact
windowed rollups, model/component price inputs, activity/outcome costs, chart
buckets or snapshot metadata. `localUsageRepository/list` is lifetime-only and is
never used as a substitute for a requested time window. Existing source summary
support therefore proves the transport and accounting boundary, not completion of
the performance objective. The API mode is opt-in until a real producer build
passes the recorded integration journey.

CodexMulti task `pc20ab013b8a118e7` owns the remaining producer work. Coordinator
outcomes `p95cabc166aa543e4` and `p75bdcffb374e4e6c` remain open until populated cold
and warm Usage/Performance journeys meet the under-one-second target and the
256 MiB usage memory budget with real source data, concurrent navigation, collector
failure/recovery, all supported ranges, and exact-window reconciliation. A new
cache without those measurements is not completion evidence.
