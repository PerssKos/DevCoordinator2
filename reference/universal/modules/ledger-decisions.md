## 4. Keep decisions, unfinished outcomes, and executions separate

- Use the configured software-owned database as the authoritative
  completion ledger and decision history, including user feedback and
  durable corrections. Access it through the configured coordinator's
  supported tools; never substitute files or chat memory. Database
  unavailability blocks the affected completion claim, not independent
  authorized work.
- Record consequential decisions with `decision_record`: an aspect,
  management-facing title and body, materially distinct options, and
  meaningful cost and risk. Put implementation detail in `technical_note`;
  use `supersedes` when replacing a decision and a stable `ref` when cited.
- Load routine decision context with `decision_tail`. Search the full
  history with `decision_search` before retrying an option that may have
  been rejected.
- When a decision read reports `summary_due`, store the rolling summary
  with `decision_summarize` before continuing. Summarize durable direction
  and quality expectations, distinguish confirmed decisions from inferred
  patterns, and cite decision refs. This append-only maintenance needs no
  additional user approval.
- The completion ledger records durable unfinished outcomes, not execution
  attempts. Every passed, failed, cancelled, timed-out, invalidated,
  retried, or superseded run belongs in governed run history.
- Create or reopen a task only when evidence establishes a durable missing
  or regressed outcome. Execution failures and suggestions require diagnosis
  before becoming ledger outcomes; passing runs do not automatically complete
  work.
- Knowingly leaving in-scope functionality temporarily unimplemented
  establishes an unfinished outcome. Record it when the omission is decided;
  no additional investigation is needed to establish a deliberate omission.
- Reuse an existing task only when its concrete outcome and acceptance scope
  match the omitted functionality or diagnosed gap. An umbrella covering the
  same subject area is insufficient representation. Do not create a duplicate
  of an equivalent concrete outcome.
- Place the omission outcome under a fitting existing umbrella only when
  that umbrella's recorded estimate exceeds 100 lines. Otherwise, record it
  as a standalone outcome. An estimate of exactly 100 lines, or an unknown
  estimate, does not qualify an umbrella.
- The threshold controls placement only. Exceeding or crossing 100 lines
  triggers no planning, review, or decomposition. The omitted functionality's
  own size does not exempt it from recording. Apply this placement rule when
  recording or reconciling authorized omissions, without a retrospective
  backlog rewrite.
- Execution-only actions are not tasks. Missing test or harness capability
  may be an outcome; running or rerunning it is not.
- Make the remaining behavior, reason for deferral, originating requirement
  or feedback, and completion criteria explicit. Keep every agreed gap open
  until verified or explicitly removed from scope by the user. Preserve
  append-only history and keep externally blocked outcomes open. Refining an
  existing task to clarify non-obvious requirements remains appropriate; a
  temporary omission still requires concrete representation.
- Reconcile known omissions before delivery or handoff, and reference their
  task IDs when reporting limitations. Preliminary delivery may proceed
  while these outcomes remain open.
- Write tasks for a non-specialist: the remaining outcome, user impact,
  unblock condition, and observable completion proof come first.
  Technical details may follow.
- Link runs to tasks through structured evidence references. Keep compact
  run receipts after verbose evidence expires; do not copy run status,
  logs, or failure narratives into task history.
- Readiness requires both no request-related unfinished outcome and fresh
  required verification evidence.
