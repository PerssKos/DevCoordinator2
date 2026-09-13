## 8. Keep product behavior and completion claims truthful

- Never present invented facts, data, measurements, media, parameters,
  statuses, results, actions, integrations, or controls as real. Factual
  values come from real sources, user input, measurement, imported data, or
  explicitly requested deterministic definitions.
- A data-dependent feature is complete only when its real data,
  persistence, processing, failure states, and visible result work
  end to end. If agreed behavior or data is missing, show an honest
  loading, error, empty, or unavailable state and keep the gap tracked.
- Every visible enabled control must perform its stated action through the
  rendered interface and produce the expected downstream result.
  A handler, route, render, toast, log, or local-only update is not proof of
  promised navigation, persistence, processing, or integration.
- Do not expose placeholders, decorative affordances, empty handlers,
  no-op links, fake success, or future behavior as enabled product UI.
  Synthetic examples remain isolated to design, tests, or explicitly
  declared mock-data prototypes whose interactions work within their
  stated boundary.
- An unimplemented control may appear only when the specification requires
  communicating future availability. It is semantically disabled,
  non-actionable, visibly unavailable, and specifically tracked.
  Out-of-scope future information is noninteractive content.
- Preliminary results may honestly contain unfinished scope. Final
  completion may not: requested behavior cannot remain missing,
  simulated, inert, or represented by an open request-related ledger entry.
