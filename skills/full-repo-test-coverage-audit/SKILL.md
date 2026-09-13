---
name: full-repo-test-coverage-audit
description: Explicitly audit complete line/branch coverage, meaningful code and UI tests, and efficient focused execution using source-bound native evidence and a manifest-verified review.
---

# Full Repo Test Coverage Audit

Run this exhaustive, read-only workflow only when explicitly invoked as
`$full-repo-test-coverage-audit`. Ordinary implementation or test-gap questions
do not require a full audit. Keep artifacts outside the audited repository.
Use the canonical shared Rust tooling; do not copy or recreate the harness.

Report four separate facts: whether the audit is complete, whether executable
code has **100% line and branch coverage**, whether required UI journeys have
meaningful executed tests, and whether testing meets the project's efficiency
requirements. A valid report can expose gaps. Missing evidence is **unproven**,
not a passing result; source/test matching is structural evidence only.

## Prepare the evidence once

Read the project's requirements, standing corrections, test configuration and
agreed timing budgets. Reuse existing native collection/results and their
run-time source/configuration receipts. Never reconstruct a receipt after an
old test run or launch a complete suite merely to start this audit. Missing
measurements become findings or unresolved assessments. Obtain additional
execution through the project's existing authorized test workflow when needed.

```bash
devcoordinator2-tooling audit test-coverage build --repo /path/to/repo \
  --out /path/to/audit \
  --coverage-report /path/to/coverage.info \
  --assurance-input /path/to/assurance.json
```

Both evidence options are optional for diagnosis. Repeat `--coverage-report`
for LCOV, Cobertura, coverage.py JSON or Istanbul reports. The builder writes
`assurance-input.example.json` to help assemble missing scope and evidence.
Read [the evidence contract](references/evidence.md) when preparing these inputs.
Use existing runner reports; do not build another scheduler, runner or cache.

Inspect `manifest.json`, `audit_index.md`, `test_inventory.json` and
`excluded_files.json`. Resolve or
explicitly report scope warnings. Classify every source file, justify exclusions,
and retain zero-hit files in the denominator. A trivial wrapper may need no
separate unit test but still belongs in measured executable-code coverage.
Branch-free code needs an explicit measured zero-branch denominator. Unsupported
or absent instrumentation cannot satisfy the coverage requirement.

## Review without duplicate work

Assign one fresh isolated worker per generated batch when delegation is
available. Give each the complete prompt and applicable project decisions.
Every worker inherits the parent's settings; **do not set model or reasoning
overrides, inspect effort, or record effort levels**. If workers are unavailable,
use the same reports with disclosed manual fallback. Track assignments,
provenance and completion in `review_ledger.json`. Use `completed` for normal
review. For manual fallback, use `manual-fallback-completed` on the lead and
each required worker, retain the actual reviewing agent/task identity in
`agent_id`, and describe the manual work in `runtime_provenance`. Set the
top-level fallback to `{status: "completed", reason: "<actual limitation>"}`.
Never use null provenance or invent another worker.

One coordinated UI reviewer owns `ui_test_coverage_audit.md`, including component,
integration, e2e and visual evidence. Reuse the shared formal UI verifier's
existing evidence and review validation. Source workers assess wiring and test
assertions; they do not repeat rendered judgments. CLI/library packages with no
owned rendered UI may record a justified not-applicable assessment.

Workers write complete reports to their exact prompt-declared paths and return
only bounded `REPORT_SAVED` receipts. Keep verbose output in `logs/`.

For each deterministic target, inspect the actual tests and expected outcomes:

- Map its exact target ID to `TESTED`, `UNTESTED` or `NOT_REASONABLE` and retain
  the required report columns from the generated prompt. Every File Coverage
  row uses status `CHECKED` and the exact manifest SHA-256. Add overlooked behavior
  using a unique `manual-...` ID tied to the owned file/unit.
- Cite an unambiguous source declaration or an exact collected `file#test name`.
  Native collection supports expanded parameterized identities. A comment,
  filename, snapshot's existence or collection-only result is not execution.
- Review happy, boundary, invalid, failure, async, permission, persistence and
  recovery scenarios where applicable. Cite the observable assertions. Report
  missing scenarios even when an existing test earns a `TESTED` disposition.
- Label evidence `STRUCTURAL`, `EMPIRICAL`, `MANUAL` or `NONE` honestly.
  `EMPIRICAL` requires passing native evidence and source-bound complete line
  and branch measurements; hitting a declaration line is insufficient.
- Bind every `UNTESTED` target to a finding. `NOT_REASONABLE` requires a concrete
  rationale and does not itself exclude executable code from measured coverage.

The UI inventory names each required journey, control, interaction, state,
theme, viewport, expected outcome and exact test/check identity. Reconcile it
with discovered controls and requirement-derived dynamic surfaces. Check
cancellation, validation, recovery and persistence where promised. Different
configuration cells need distinguishable executed evidence. Screenshots and
geometry support interaction assertions; they do not replace them.

## Assess test efficiency

Use one categorized check/test inventory and source-bound timings. Distinguish
unit, component, contract, integration, e2e, visual, static and setup work, and
its development, pre-merge and release tier.

- Find overlapping filters/jobs/shards that execute the same test and variant
  repeatedly. Review semantic redundancy by assertions and failure modes;
  shared code coverage alone does not make two tests redundant. Preserve
  justified platform variants, distinct risks and deliberate repeatability tests.
- Review avoidable serial dependencies, actual resource conflicts, independent
  overlap and admission waiting. Capacity constraints need evidence; do not
  replace the host scheduler with local worker limits or fake dependencies.
- Assess repeated builds/seeding, fixture cost, fixed sleeps, retries/flakiness,
  and expensive browser cases better covered at a lower layer. Prefer the
  smallest meaningful test while retaining real integration and UI proof.
- Compare actual wall-clock feedback with the project's focused, pre-merge and
  release budgets. Keep concurrent work durations separate from elapsed time.
  Missing limits, measurements or review evidence remain unresolved.
- Exercise the existing selector with local, shared, UI, fixture, configuration
  and unmapped changes, or justify a non-applicable case. Include transitive
  dependencies and prerequisites. Unmapped changes broaden selection; extra
  checks need a concrete reason. Validate that small changes get focused tests.

Focused tests belong during edits; broader testing belongs after coherent
integration; a fresh complete run proves the final candidate. Do not count
required final validation as waste or infer speed from configuration alone.

## Verify and deliver the findings

```bash
devcoordinator2-tooling audit test-coverage verify \
  --manifest /path/to/audit/manifest.json --reports /path/to/audit/reports
```

`ok` describes report integrity/completeness. The separate `coverage`, `ui` and
`efficiency` verdicts and `assurance_met` describe the requirements above.
`assurance-report.json` retains full measurements, findings and unknowns. Add
`--require-assurance` for automation that must fail unless all requirements are
met: exit 1 means invalid audit evidence, 2 setup failure, and 3 a valid audit
with gaps or unresolved assurance. Skipping source freshness cannot pass this gate.
Legacy audit artifacts stay readable without their obsolete effort gates and
cannot acquire stronger assurance merely by being reverified.

Reconcile source, UI and efficiency findings; inspect high-impact claims as the
lead. Software checks identities, scope and evidence consistency; reviewers
remain responsible for the truth of requirements and assertion-quality judgments.
Write `final-report.md` with Coverage, Test Architecture Findings, UI And Journey
Coverage Findings, Function And Method Coverage Findings, Implementation Plan,
and Verification Plan sections. Preserve actual measured scope and all unresolved
outcomes. Prioritize missing core behavior or unsafe failure paths above cleanup;
rank efficiency changes by measured feedback benefit and risk.

Return a compact outcome, priority/count summary, independent verdicts,
limitations and artifact links. Do not paste complete reports or raw logs.
