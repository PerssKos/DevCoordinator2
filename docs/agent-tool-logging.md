# Agent tool request logging

DevCoordinator2 emits a privacy-safe lifecycle record for each request that
reaches its local protocol daemon. It also records MCP validation attempts that
fail before a socket request is sent. These records are written through the
daemon's structured tracing output under the `devcoordinator2.agent_tool`
target, so they are available in the service's stderr or journal without a
second database or completion ledger.

Each daemon request produces a start and finish event. The finish event has a
short process-local `request_ref`, operation name when it was parseable, client
kind, model/effort labels, request/response byte counts, work-context presence,
phase, terminal status, bounded duration, and a typed error code when the
protocol returned one. MCP validation events use a separate short `mcp_ref` and
record the tool name, validation/return phase, status and duration.

Statuses distinguish `succeeded`, `rejected`, `transport_failed`, and
`incomplete`. Phases identify read, parse, dispatch, wait, write, shutdown,
peer-credential, or MCP validation boundaries. A malformed frame, admission
rejection, handler error, cancellation, disconnected wait, and failed response
write all remain visible as terminal observations.

The log projection deliberately omits protocol request IDs, request
parameters, paths, identities, credentials, response bodies, and error detail.
Use the bounded typed error code and operation name for aggregation; retrieve
private cold evidence only through an explicitly authorized diagnostic path.

The `devcoordinator2.agent_tool=info` filter is enabled by default even when
the general tracing filter remains at its normal error level. `RUST_LOG` can
still add other targets without changing the privacy boundary.

These logs cover calls that reach DevCoordinator2. Direct agent-side failures
that occur before an MCP/CLI request is sent require instrumentation in the
agent runtime; that change belongs in the CodexMulti repository and is tracked
by the separate handover prompt.
