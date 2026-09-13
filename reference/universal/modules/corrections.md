## 10. Preserve lessons from confirmed agent mistakes

- Distinguish agent mistakes from changed user intent, user input, and
  external state using the request, clarifications, accepted plan,
  project records, and delivered behavior.
- For a confirmed mistake, reproduce the user's surface where feasible and
  finish its useful diagnostic cycle. Identify the misunderstanding,
  implementation gap, or verification assumption and the nearest durable
  prevention layer.
- Before the product fix, locate the relevant feedback, unfinished outcome,
  and standing correction through the coordinator's planning and decision
  tools. Reuse existing outcome tasks and append or supersede the durable
  correction with stronger prevention and verification; preserve its record
  references. If immediate mitigation prevents harm or data loss, preserve
  evidence and mitigate first.
- Batch the guardrail and implementation changes. Inspect only plausibly
  adjacent paths, then retest the original surface, guardrail, adjacent
  cases, and completion-ledger state.
- Keep generalized repeatable lessons in policy and narrow guarantees in
  requirements, acceptance criteria, tests, verifiers, harnesses, or
  operational checks. Keep one-off narratives out of policy.
- Keep unresolved user feedback in the authoritative task ledger. Record a
  confirmed, repeatable correction through `decision_record`, linked to the
  relevant feedback or outcome task when one exists. A completed task does
  not retire its standing correction; feedback alone is not proof of an
  agent mistake.
- Give each correction explicit applicability, the confirmed mistake
  pattern, required behavior, prevention and verification, and references
  to its supporting feedback or evidence. Keep the user-facing account
  understandable and put implementation detail in `technical_note`.
- Use supported decision aspects and explicit applicability to keep
  corrections narrowly scoped: UI, automation, coding-style, math, data,
  security, operations, testing, documentation, and each affected business
  perspective. Do not invent tool fields or maintain parallel file ledgers
  to express a scope the service can already record.
- Cite service record IDs and stable refs. Preserve legacy correction IDs
  as provenance and search them before creating a duplicate; file paths no
  longer own correction identities. Strengthen a lesson through a linked
  append-only record or `supersedes`, retaining the original history.
- Do not classify changed intent, new scope, external failures, or
  unconfirmed agent-found concerns as confirmed mistakes. Those may belong
  in their appropriate task, decision, or execution records instead. Keep
  raw conversation and incident narration out of durable corrections.
- Corrections remain discoverable after fixes. Retraction or replacement
  requires an explicit recorded decision and preserved history, not task
  closure or deletion of the earlier record.
- Legacy file-based ledgers are historical reference material, not another
  writable authority or an offline fallback. Do not create or update them,
  delete historical files merely because authority moved, or claim their
  contents were migrated without checking the authoritative records.
- Keep durable prevention in decision history, unfinished outcomes in the
  task ledger, and execution or incident evidence in its governed history.
  Link these records without treating one as a substitute for another.
