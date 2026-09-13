## 13. Review outcomes and resource use every 24 hours

- For each project with continuing work, including performance-only
  specifications and research, complete a review after the first
  24 elapsed hours and every subsequent 24 hours while work remains active.
  This review cadence is separate from the project's delivery watchdog
  intervals; overriding those intervals does not silently change the review
  cadence. Complete an overdue review before starting another implementation
  batch. A review does not reset a delivery deadline.
- Preserve review boundaries across sessions and agents. State the actual UTC
  reporting interval, cover work not yet reported, and identify any inactive
  or unobserved periods rather than dropping them or inventing continuous work.
  Coordinate one project review across its tasks and participating agents.
- Start with results against the original specification and approved changes:
  what users can actually do, the URL or downloads to inspect it, which agreed
  journeys have observable evidence, what remains missing, and any divergence.
  Explain missing progress directly. Activity lists, infrastructure work,
  screenshots, or scaffolding do not substitute for promised working behavior.
- Report resources for each individual task using its stable identity and
  understandable intended outcome: measured tokens, active agent time,
  elapsed time, and waiting time where available. Include delegated work,
  retries, rework, testing, and delivery effort within the reporting interval.
  Explain what that effort achieved, not merely which tools were invoked.
- Obtain figures from the available authoritative usage and execution tools.
  Preserve their coverage and provenance. Do not double-count parent and
  descendant totals, overlapping wall-clock intervals, or token categories
  that the source identifies as components of another total. Distinguish
  concurrent agent effort from elapsed time and measured tokens from estimates.
- Publish measured totals and explicit missing or unattributed figures when
  precise task attribution is unavailable. Do not fabricate allocations or
  omit the review. Measurement gaps alone do not block further development;
  explain their effect on the assessment and improve accounting through
  supported, authorized means.
- Assess whether the resources produced useful progress toward the agreed
  outcome. Examine repeated discovery or context loading, duplicated work,
  excessive broad validation, avoidable rebuilds, serial waits, handoff costs,
  and rework caused by misunderstood requirements. Identify evidenced causes,
  not generic optimization suggestions or unsupported claims of optimality.
  User waiting, inactive periods, and intentional required release revalidation
  are not waste by themselves. Distinguish necessary repeated validation of a
  changed frozen candidate from avoidable reruns of an unchanged one.
- Prioritize elapsed time to the next useful result, then resource efficiency;
  never trade away agreed scope, correctness, or required verification. Trigger
  an additional review on an evidenced bottleneck, deduplicated by scope, cause,
  and relevant evidence revision. Do not rerun an unchanged review on each wake
  or duplicate a daily review covering the same cause and evidence.
- Choose concrete process improvements that reduce token use or delivery time
  without shrinking the agreed result or weakening required verification.
  Automatically implement only within the reviewed repository and current
  authorized scope; continue useful work and compare the observed effects in
  the next review. Specification-only work authorizes improvements to that
  work, not product implementation. This automatic review loop never expands
  repository scope. The separate dependency-repair workflow below requires
  its own applicable authority and repository review; it is not an automatic
  cross-repository optimization.
  When the current process is appropriate, explain the evidence instead of
  inventing changes merely to fill the report.
- If a necessary, evidence-backed improvement requires changing DevCoordinator,
  apply the owner's recorded standing authority as well as task-specific
  authorization. Trusted local agents covered by standing development and
  dependency-repair authority may diagnose, repair, verify, and install the
  necessary fix without another conversational approval. A repository boundary,
  bug report, or repeated failed check does not revoke that authority. Preserve
  the original outcome, use the Coordinator's canonical-source and reviewed
  non-self-hosting workflow, and verify the affected surface. Continue independent
  authorized work; never introduce a competing controller or bypass mandatory
  host/tool controls. Only when the required change falls outside confirmed
  authority, stop dependent work and report the concrete limitation, proposed
  change, expected benefit, and smallest necessary user decision.
- Optional infrastructure ideas and measurement gaps alone do not trigger this
  stop. While a required Coordinator decision is outstanding, preserve running
  results and evidence, finish useful bounded diagnosis and reporting, and let
  independent projects continue. Do not use the review to start an unrelated
  infrastructure project.
- Deliver the review to the user in understandable task-oriented language,
  with the next intended user-visible result and its deadline. Retain compact
  report and evidence references through the supported authoritative records;
  put consequential optimization decisions in decision history. Do not create
  a new completion task for every scheduled review or copy verbose run logs into
  task history. Keep verification criteria separate from execution receipts.

### Improvement decision protocol

For each daily or bottleneck review, retain a compact, evidence-linked record:

1. **Hypothesis:** name the observed delay, affected outcome, scope, baseline,
   evidence and uncertainty; do not infer waste from totals or elapsed time.
2. **Options:** compare materially different actions, including keeping the
   current approach, with expected speed benefit, cost, risk and authority.
3. **Chosen action:** explain the choice, repository and scope boundary, owner,
   success measure and reversible keep/revert condition before changing work.
4. **Measurement:** compare the same observable outcome before and after; retain
   actual tokens, active/elapsed/waiting time, coverage and source references.
5. **Keep/revert:** keep demonstrated improvement, revert an ineffective or
   harmful change safely, or mark evidence inconclusive and define the next
   bounded comparison. Link the next review; never report only resource totals.

Use supported decision records for choices and governed execution receipts for
measurements. This checklist is a protocol, not a new writable file ledger.
