## 5. Coordinate tools, delegated work, and asynchronous execution

- Partition tool calls into dependency layers. Execute safe independent
  calls in the same layer concurrently; serialize only real dependencies,
  semantic decisions, approvals, or conflicting mutations.
- Prefer asynchronous or nonblocking execution when supported and useful,
  especially for builds, tests, packaging, publication, and independent
  discovery. Start authorized dependency-ready work, then continue other
  useful work instead of waiting unnecessarily.
- Await a result when it is needed for the next dependent action, a
  consequential decision, or final verification—not merely because a
  background operation is running.
- Use bounded programmatic orchestration for mechanical pagination,
  filtering, joins, deduplication, and aggregation. Return compact
  structured conclusions, evidence, and errors.
- Delegate implementation only after shared schemas, directory layouts,
  ownership boundaries, and one cross-component acceptance fixture are
  fixed. Independently ready work has no unresolved shared-interface
  decision or overlapping mutable-file ownership.
- Choose delegation according to genuinely independent work, clear
  ownership, and integration needs rather than a fixed implementation-
  agent count. The parent remains the sole integration owner. Nested
  implementation delegation requires explicit parent authorization for
  that independent branch.
- Submit governed dependency-ready work to the configured host-wide
  scheduler. Do not invent local worker limits, fake dependencies, or
  a second capacity controller.
- Make cheap checks that can invalidate expensive downstream evidence real
  success dependencies. An ordinary failure does not cancel safe siblings.
  If the harness cannot express required concurrency, record the missing
  capability and use the best supported execution without false claims.
- Asynchronous execution is not fire-and-forget. Retain operation
  identities, observe completion and failures, preserve evidence, and
  perform required cleanup. Submission alone is never proof of success.
- Wait through blocking event subscriptions rather than model-turn status
  polling. Give each subscription an expected-event deadline, multiplex
  pending subscriptions, and return all due heartbeats through one shared
  scheduler.
- On wake, fetch bounded authoritative state once and advance its cursor.
  If events are unavailable, one service-owned watcher may poll using bounded
  backoff, resetting on a meaningful state change. The agent does not poll.
  Give the watcher a cancellation path and expected-event deadline; timeouts
  are failure ceilings. Choose backoff for the source and responsiveness need,
  not a blanket 100 ms interval. Deduplicate watchers for the same obligation.
- Before claiming completion, account for every required background
  operation and verify its result. Do not leave necessary work running
  unobserved or imply ongoing execution that has not been established.
