---
name: full-repo-audit
description: Run a manifest-verified repository implementation, source, interface, and user-journey audit with semantic end-to-end contract tracing, deterministic file batches, explicit lead rechecks, real hashed visual evidence, and honest runtime capability/effort provenance. Use for incomplete features, hidden implementation gaps, hard-coded substitutes, partial plumbing, TODO/stub behavior, architecture and security risk, misleading UI promises, missing tests, and prioritized repository-wide completion ledgers or plans.
---

# Full Repo Audit

## Overview

This exhaustive workflow is explicit-only. Run it only when the user invokes
`$full-repo-audit`; ordinary implementation, review, testing, or gap-finding
requests must not activate it implicitly.

The installed skill must be a direct link into the canonical DevCoordinator2
checkout. Its scripts resolve that link and import the one root
`full_repo_harness`; copied standalone skill packages are unsupported.

Run a multi-level audit: the lead agent performs the architectural, product-flow, implementation, and interface review; low-effort subagents manually inspect every source file in deterministic batches; separate UI journey workers inspect source-level journeys and visual testability. Use the harness script to create a coverage manifest and worker prompts, then reconcile file traces into complete feature and entry-point traces before producing the final implementation plan.

Treat documented product intent, confirmed user journeys, source-backed feature promises, visible UI elements, and expected tests as one complete product contract. Every intended journey, feature, route, control, state, handler, persistence path, permission path, error path, and verification path must be present, implemented, and tested; otherwise report the gap.
A checked file and matching hash prove review coverage, not implementation completeness. Build a source-backed inventory of every promised feature and public or operational entry point, assign every distinct file-level responsibility a unique stable audit-local `Contract ID`, then manually trace each item through all applicable implementation layers. Do not equate the absence of `TODO`, `NotImplemented`, placeholder vocabulary, or test failures with completeness. Present symbols, registrations, and tests are leads; prove what the code actually calculates, persists, calls, changes, returns, exposes, and recovers from.

For every contract item, explicitly account for `entry-registration`, `core-logic`, `data-lifecycle`, `integration-boundary`, `authorization-trust`, `failure-recovery`, `observable-outcome`, `operational-lifecycle`, and `verification` as `pass`, `gap`, `blocked`, or `not applicable`, with concrete source or runtime evidence. Any `gap` or `blocked` result needs a finding. A clean implementation claim requires every inventoried contract item to be accounted for; UI review cannot substitute for this trace.
Every batch responsibility records why its expected behavior is authoritative or
honestly source-inferred, how it was discovered, and a verification
counterfactual or invariance. Source-only evidence cannot close a PASS for
persistence, integration, external effects, or success. Workers own file-local
semantic inspection. The lead rechecks cross-file contracts, high-risk paths,
gaps, blocked or ambiguous evidence, and stateful/external PASS claims; it
reconciles every other verified worker contract without rereading the same
file-local proof. Sampling is not a substitute for reconciling the complete
contract inventory.
Hard reporting gate for every repo: the final audit's `## Implementation Findings` must contain those nine literal trace labels with evidence-backed statuses for every inventoried `Contract ID`, or a compact table that maps every item to all nine statuses. A clean audit still includes the complete trace table, followed by `No confirmed findings.`; the sentinel never replaces the table. If any item or label is absent, rewrite the final report before returning it.
Hard reporting gate for UI-bearing repos: the final audit must contain the
literal checklist labels `badge-detail`, `row-hit-target`,
`navigation-cursor`, `transient-disclosure`, `disclosure-scrollbar`,
`icon-meaning`, `stable-expansion-width`, `hover-copy`, `status-summary`, and
`message-metadata` in `## Interface Findings`, `## User Journey Findings`, or
`## File-Level Findings`. Mark each `pass`, `gap`, `blocked`, or `not
applicable` with evidence. If any label is absent, rewrite the final report
before returning it.
Do not let source-inferred UI structure, audit-generated documentation, or an
over-prescriptive handoff doc become confirmed product truth. When a UI-heavy
repo has unclear journeys or screenshots show overloaded/default-detail-heavy
surfaces, report the UI assumption status as `source-inferred` or `missing` and
escalate to screenshot-based visual review. Invoke `ui-implementation-audit`
only after manually confirming and naming a substantive repo-owned target UI
source file and passing that skill's recognized-source preflight; UI-heavy
dependencies, arbitrary source paths, mockups, screenshots, stories, tests,
and planned screens alone do not qualify. Use its exceptional type-to-component
source-anchor relationship only when manual inspection proves a real
unrecognized UI toolkit.

## Required Execution Model

- Request extra-high reasoning effort for the lead audit. Call it `runtime-attested x-high` only when immutable/runtime provenance actually reports that setting. A configured or self-reported value must be labeled `ledger-recorded-unverified`; never call it confirmed. If runtime attestation is unavailable, the audit may continue only as `unattested lead review`, must disclose that limitation, and must not claim x-high assurance.
- Use fresh isolated workers for file-batch inspection. Let the runtime or user
  select worker effort. Pass the entire generated prompt and every applicable
  project-ledger requirement; do not rely on an inherited lead transcript.
- When interface-relevant files are queued, use separate low-effort workers for `journey_audit.md` and `visual_journey_audit.md`. Manual fallback may process these prompts in the current agent only when worker spawning is unavailable, but the final coverage label must disclose that fallback.
- Treat the user's request to run this audit as authorization for the batch subagents required by this workflow.
- After the lead x-high gate above is satisfied, inspect the active runtime's
  worker-context and effort capabilities. Request Light/runtime `low`; the
  label difference is not an unsupported-effort condition. Enter manual
  fallback when runtime `low` is genuinely unavailable, workers cannot be
  isolated from the parent transcript, or spawning is unavailable/fails.
- Keep a lead-recorded effort/capability ledger. Every capability and effort claim needs a claim basis (`runtime-attested`, `tool-schema-inspected`, `self-reported`, or `manual-fallback`), a consistent claim label, and concrete provenance/evidence. The verifier checks ledger consistency, not hidden scheduler state.
- If manual fallback is required, disclose the exact limitation, run each
  generated batch sequentially in the current agent, save one report per batch
  under `<audit-output>/reports/batch_###.md`, verify coverage with
  `devcoordinator2-tooling audit verify-full-repo`, and label final coverage as `manual fallback
  coverage` rather than subagent coverage.
- Keep complete worker reports, lead reconciliation, verifier output, command
  stdout/stderr, and final synthesis in `<audit-output>/reports/`,
  `<audit-output>/logs/`, and `<audit-output>/final-report.md`. A worker writes
  its prompt-declared report directly and returns only the bounded
  filename-bearing `REPORT_SAVED` receipt. Never route a complete report or raw
  log through the subagent response or final chat message.
- Keep subagent ownership disjoint: one batch prompt per subagent, no overlapping file ownership unless a batch must be rechecked.
- Keep the audited repo read-only by default. Every verified artifact-backed
  audit creates and fully reviews an exact external completion-ledger
  projection, including an empty projection for a clean audit; creating that
  artifact does not authorize repository mutation. An explicit user request to
  record, write, update, or reconcile audit findings—or an applicable project
  instruction that already requires active findings to be recorded—permits
  only a transactional import through the authoritative software-owned
  database ledger; it does not authorize implementing findings, changing any
  source, committing, or pushing. Place audit artifacts outside the audited
  repo and disclose their path. Never create or update `CompletionLedger.md`.
- If the user explicitly forbids all file writes or artifact creation, do not run the harness or save reports unless they authorize audit artifacts. Perform a chat-only manual audit, label coverage as `chat-only unverified coverage`, and state that manifest/report verification was not run.
- Do not claim every file was checked until the manifest coverage and subagent coverage reports match.
- Never write raw worker or lead findings directly to any ledger store. Verify the batch, journey, visual, and `reports/lead_reconciliation.md` artifacts; reconcile cross-file traces; directly review every candidate; deduplicate by completion outcome; and dispose every candidate before generating an authorized database import.

## Workflow

1. **Set scope**
   - Use the current working directory as the repo root unless the user names another path.
   - Include tests, build scripts, extensionless scripts/hooks under script directories such as `scripts/` and `.husky/`, lockfiles, project/build files, schema/config files, routes, jobs, migrations, native UI files, source-backed UI assets, UI components, templates, translations, message catalogs, journey/workflow docs, and operational Markdown such as `SKILL.md`, `AGENTS.md`, and repo docs that define expected behavior.
   - Exclude generated/vendor/build artifacts unless the user explicitly asks to audit them; pass `--include-generated` and/or `--include-vendor` only for that explicit request.
   - Exclude real `.env` files by default; use `--include-env` only when the user explicitly wants secret-bearing environment files audited.

2. **Generate the audit queue**
   - Resolve the audited repo to an absolute path before generation and echo it in your working notes.
   - Resolve `FULL_REPO_AUDIT_SKILL_DIR` to the directory containing this `SKILL.md` from the loaded skill path, then resolve `CANONICAL_SKILL_ROOT` as its repository root. Preflight the Rust harness before starting with `devcoordinator2-tooling skills self-test audit-tooling --source-root "$CANONICAL_SKILL_ROOT"` unless the user forbids validation commands. Cargo reuses only exact source, dependency, toolchain, and platform artifacts; drift recompiles the affected tests. If the self-test fails, stop instead of treating it as an audited-repo finding.
   - Run:
     ```bash
     REPO_ROOT="${REPO_ROOT:-$PWD}"
     devcoordinator2-tooling audit build-full-repo --repo "$REPO_ROOT"
     ```
   - If the user names a repo path, set `REPO_ROOT` to that path instead of `$PWD`.
   - By default, output goes under a unique system temp directory, not inside the audited repo.
   - Use `--out <dir>` when the user wants a specific artifact location.
   - Treat `--out` as a harness-owned artifact directory. The harness writes `.full-repo-audit-artifacts.json` and refuses to clean non-empty directories that are not marked as harness-owned.
   - Use `--include-generated`, `--include-vendor`, or `--include-env` only when the user explicitly wants those normally excluded artifacts audited.
   - Use `--include-assets` when the user explicitly wants binary UI assets such as logos, icons, screenshots, or fonts audited; without it, likely UI assets are excluded with scope warnings that must be resolved before full coverage is claimed.
   - Use `--include-file <repo-relative-path>` or `--include-glob <glob>` to force a source/manual batch for a high-signal unknown file that was raised as a scope warning.
   - Use `--run-id <stable-token>` only when an external orchestrator or repeated audit loop needs artifacts to share a caller-supplied run id; otherwise let the harness generate one.
   - Use `--batch-size` and `--max-batch-bytes` to keep batches small enough for manual inspection. Oversized UTF-8 multi-line files are split into line-range coverage units; long/minified single-line or non-UTF-8 files are split into byte-range coverage units. Dispatch and verify each unit instead of treating a very large file as one undifferentiated worker task.
   - Confirm `queue_complete.json` exists, its `phase` is `queue_generated`, and its `run_id` matches `manifest.json` before dispatching subagents. This marker means queue generation completed; it is not proof that the audit reports are complete.
   - Confirm the manifest's `lead_reconciliation` entry points to generated `<audit-output>/lead_reconciliation.md`; this is the mandatory prompt for the lead's verified cross-file report, not an optional note.
   - Fill `effort_ledger.json` as you dispatch and reconcile: subagent capability check, spawn tool, capability notes/evidence, lead effort status, lead-reconciliation status/report, fallback status, pruned-directory review status/notes when required, journey worker agent id/effort/status/report, per-batch agent id/effort/status/report, per-batch runtime provenance, and the ledger's self-reported provenance scope.
   - Review every `manifest.json.high_risk_files` entry directly as lead. Complete the exact `lead_high_risk_review.files[]` row with the manifest hash, risk reasons, concrete source evidence, and security/data-loss/recovery notes. Batch-worker review alone cannot close a high-risk file.
   - Inspect `excluded_files.json` after generation and resolve any `scope_warning: true` rows before claiming full coverage. If `manifest.json` has `pruned_directory_review_hint_count > 0`, inspect `pruned_directory_review_hints`, requeue first-party source-like samples when needed, and record one structured `pruned_directory_review.decisions[]` entry per hinted path in `effort_ledger.json` using `requeued`, `excluded-with-rationale`, or `out-of-scope-with-user-confirmation` plus a concrete rationale.
   - A scope warning is verifier-cleared only by re-running the queue with the needed include flag or requeuing the specific file in a new/manual batch. If the user confirms an exclusion or the lead downgrades the final coverage claim, keep the verifier failure visible and list the exclusion explicitly.

3. **Run the lead architectural pass**
   - Inspect the repo structure, docs, package/build files, routes, schemas, tests, deployment config, entry points, and domain terminology.
   - Identify intended product surfaces, feature families, UI elements, user journeys, and test expectations before reading subagent findings.
   - Build an implementation-contract inventory from requirements, acceptance criteria, help and visible promises, public APIs/package exports, commands, routes, event consumers, scheduled jobs, workers, configuration, schemas/migrations, feature flags, build/install/package paths, deployment/upgrade/backup/recovery paths, and operational documentation. Give every distinct file-level responsibility row its own unique stable audit-local `Contract ID`; never reuse an ID across rows. The lead reconciliation groups the relevant IDs when one end-to-end contract crosses files or batches.
   - For every contract item, trace `entry-registration`, `core-logic`, `data-lifecycle`, `integration-boundary`, `authorization-trust`, `failure-recovery`, `observable-outcome`, `operational-lifecycle`, and `verification`. Mark each `pass`, `gap`, `blocked`, or `not applicable` with concrete `path#symbol`, configuration, test, command, or runtime evidence. Treat dynamic/reflection/generated registration as requiring runtime or generated-output evidence rather than assuming it works from source shape.
   - For each responsibility, identify its contract basis as `user-requirement`, `acceptance-criterion`, `recorded-decision`, `public-contract`, `interface-promise`, `caller-contract`, `schema-invariant`, `operational-contract`, or honestly `source-inferred`, and cite the concrete reference. State a counterfactual whose changed input/configuration/state/dependency/failure should change the result, or an invariance that explains why the result must remain constant.
   - Compare intended semantics with actual behavior even when the code looks finished. Look for ignored parameters or parsed configuration; constants standing in for required calculations, selection, identifiers, status, or state; functions that always return success/defaults; branches or lifecycle phases that never run; route/job/export implementations that are never registered; adapters or plumbing that transform data without invoking the real dependency; local-memory or temporary writes presented as durable persistence; read and write paths that do not meet; swallowed errors; presentation-only authorization; production paths backed by fixtures, synthetic data, null adapters, or mocks; missing migrations/rollback/retry/cancellation/cleanup; and tests that assert only type, shape, or status without proving the intended outcome.
   - Do not label an intentional constant, pure function, abstract/protocol method, documented null adapter, unsupported-platform error, test-only fixture, or read-only path incomplete without contract evidence that more behavior is required. Record ambiguity as an open question, not a confirmed finding.
   - Look for cross-cutting risks: missing integration paths, incomplete intended features, inconsistent state models, auth/permission drift, data migration gaps, error handling, observability, accessibility, security, performance, reliability, and test strategy.
   - Build an interface inventory from routes, pages, components, templates, command palettes, menus, forms, message catalogs, and visible copy.
   - Trace important controls and messages to handlers, state transitions, API calls, persistence, navigation, permission checks, and error/loading/empty states.
   - Find the repo's clear user journey description(s). Treat journey discovery as the basis for UI relevance, not as a documentation-only check. If missing, draft the most reasonable frequent journeys from app intent, routes, visible copy, and code, mark them as `draft-needs-user-confirmation`, and ask the user to confirm the most frequent use cases before treating them as final. If the audit must continue without user input, downgrade UI/user-journey coverage to `journey assumptions unconfirmed`, carry the drafts as open questions and implementation-plan assumptions, and do not state that the interface is user-friendly or convert source-inferred layout into product truth.
   - Audit all confirmed and drafted journeys, not only the single most obvious happy path. For each journey, identify the target user, goal, entry point, primary route sequence, critical decision points, rare/occasional details, destructive or heavy actions, and success/failure end states.
   - For each screen or step in each journey, compare what the user currently sees against the journey decision model: primary decision, required facts, warning/flag conditions, frequent actions, secondary/rare actions, information importance, access expectations, and unconfirmed assumptions. Do not invent a final layout from source hints alone.
   - If docs or earlier audits require many always-visible fields but do not
     explain why they are default decision content, treat that as an
     over-prescribed UI assumption. Do not preserve the overloaded layout as a
     requirement; require confirmation or a disclosure model.
   - Check compactness, fit, and layout discipline on desktop, native, and narrow/mobile surfaces: critical-always and primary-frequent information should remain usable without accidental horizontal scroll, overlap, cropping, unreadable compression, hidden overflow with no scroll path, nested visual frames, inconsistent gutters, unstable disclosure controls, or low-relevance content dominating the decision path.
   - Check interaction affordances and metadata relevance: decision badges and flags should expose hover/focus/click feedback and useful detail access when interactive; meaningful rows should not require a tiny icon-only target; elements that navigate to another screen, row, dialog, or detail surface should expose a predictable destination and pointer/focus affordance; temporary popovers and expanded panels should have an intentional close or timeout lifecycle; expand/collapse controls should not fight scrollbars; expanded and collapsed tool/result blocks should keep stable widths; copy controls should be hover/focus revealed only when they would otherwise clutter message reading and must remain reachable; concise status blocks should avoid duplicate status/severity/duration noise; unclear icons, noisy author/routing labels, selectable passive timestamps, and over-visible metadata should be treated as journey-usability defects.
   - For every UI surface that contains badges, flags, expandable rows, scrollable details, message streams, tool/result blocks, or icon-only controls, explicitly record an interaction checklist status for: `badge-detail`, `row-hit-target`, `navigation-cursor`, `transient-disclosure`, `disclosure-scrollbar`, `icon-meaning`, `stable-expansion-width`, `hover-copy`, `status-summary`, and `message-metadata`. Mark each `pass`, `gap`, `blocked`, or `not applicable`; any `gap`/`blocked` item needs a finding rather than being folded into a generic layout note.

4. **Dispatch low-effort workers**
   - Open the generated `audit_index.md`.
   - For each `batch_###.md`, spawn one fresh isolated worker, pass the entire
     prompt file content plus applicable project-ledger requirements, and use
     the runtime/user-selected effort. The generated prompt contains the
     exact report path. Accept only its compact receipt; inspect the report from
     disk rather than asking the worker to return it again.
   - Require at least one `Implementation Inventory` row for every owned file or range unit and one row for every distinct contract responsibility found there. A unit may therefore appear in multiple rows. Multiple internal definitions may share a row only when they jointly implement the same outcome and the trace names each anchor; separate entry points and independently observable outcomes remain separate rows. Each row has a unique stable `Contract ID` in deterministic `batch_###:C###` namespace, numbered within its owning batch, plus concrete source anchors, ordered implementation/data/side-effect flow, failure/permission/recovery handling, verification evidence, and `PASS`, `GAP`, or `BLOCKED`; marker searches, one catch-all file row, or a generic purpose summary are not substitutes. In the responsibility cell, require an enumerated `Basis: <kind> — <reference>` and `Discovery: parsed|manual — <assigned-unit anchor>`. Parsed discovery requires recognized named definitions; declarative, unsupported, and unparsed responsibilities require manual enumeration, and inability to enumerate is `BLOCKED`. Begin all three trace/evidence cells with `pass`, `gap`, `blocked`, or `not applicable`; derive the row result from them, and include every high-confidence named source definition in a contract row so an omitted entry point cannot hide behind a working helper.
   - When generated, spawn a separate low-effort source journey worker with `journey_audit.md`. This worker checks journey documentation, drafts missing journeys, estimates UI element relevance, and traces navigation/decision information through UI source.
   - When generated, spawn a separate visual journey worker with `visual_journey_audit.md`. If the repo is UI-heavy or screenshots are available, the lead must review the visual findings directly; do not let low-effort visual notes be the only basis for declaring a UI usable. Rerun the dedicated `ui-implementation-audit` only after manually confirming and naming a substantive repo-owned target UI source file and passing its recognized-source preflight. If no such implementation exists, keep the UI state planned or unimplemented and do not invoke that skill; reserve its type-to-component source-anchor relationship for a manually proven unrecognized UI toolkit.
   - Tell subagents not to edit files and to report coverage for every listed unit. Use the generated repository-wide `test_evidence_index.json` before searching for verification. Inspect only named candidate tests or make one targeted manifest search when the index cannot resolve a behavior; never repeat an unbounded whole-repository test search in every batch. Adjacent callers/callees may be inspected for context, but findings remain bound to assigned files. Fixture browser tests can substantiate client UI behavior but not worker/database persistence. A partial TAP stream is not a test outcome: retain a final plan/summary and exit status, rerun within a bounded command when necessary, and distinguish a concrete failed assertion from unavailable completion evidence. Package-manager installs and reification commands, including `npm ci`, are workspace mutations even when invoked for verification: run them only in an isolated copy or serially, never alongside tests or linters that resolve from the same dependency tree. Apply language-, database-, and browser-specific fallback guidance only when the assigned batch uses that runtime or boundary. An execution-environment failure is not a product gap: make one bounded, non-destructive isolated-output/cache attempt first. If it still cannot run and no concrete product failure exists, record one atomic audit-execution `BLOCKED` finding with the exact command and unblock condition.
   - Before declaring database verification unavailable, inspect the declared coordinator/runtime configuration, `GNT_TEST_DATABASE_URL`, `DATABASE_URL`, and `PG*` environment inputs, plus retained manifest-owned canonical test commands or audit evidence. A missing default socket or localhost probe is not proof that the declared database is unavailable. When an explicitly disposable database is available, make one bounded connection/test attempt against that exact target and retain the command and completed result; never probe, mutate, or treat a shared database as a disposable fixture.
   - A browser-test `PASS` is valid only when the current batch executes that browser command or cites a prior completed manifest-owned report with its exact command and final successful TAP/test summary. The verification cell must record the bounded command, a nonzero passing count, and zero failures. Source inspection, a fixture-harness definition, or a browser test that stopped at navigation is never a browser behavior PASS; record the affected contract as an atomic audit-execution `BLOCKED` result instead.
   - Before accepting a returned batch, require the worker to run a per-report verifier check: `devcoordinator2-tooling audit verify-full-repo --batch-id <batch_###> --manifest <manifest> --reports <exact-report> --json`. This scoped mode validates only that batch's source hashes, schema, anchors, findings, and inventory; it deliberately skips incomplete global reports and the pending effort ledger. Requeue malformed reports before marking their effort-ledger row complete.
   - Before every wave, inspect `effort_ledger.json` and the exact report path for every candidate. Dispatch only `pending` batches; a `completed` row with its report present is not eligible for redispatch unless the verifier requeues it. Record the worker agent ID, actual effort, in-progress status, provenance, and claim fields in one ledger mutation at dispatch, then reopen that exact row and assert the spawned ID/status before proceeding. Mark it complete only after the per-report gate passes.
   - Run in waves if the repo is large; keep a ledger of batch id, agent id, status, and returned checked files.
   - If subagents are unavailable, use manual fallback mode: process each `batch_###.md` and generated journey prompt yourself, save the required reports under `<audit-output>/reports/`, and keep the reduced coverage label through the final report.

5. **Reconcile coverage**
   - Compare `manifest.json` source files, coverage units, run id, and SHA-256 fingerprints with every subagent `File Coverage` table. For range units, the coverage row must use the exact unit id such as `path#Lstart-Lend` or `path#Bstart-end`; the exact unit id must also appear in `Findings` or `No Finding Notes` so the verifier can prove that specific range was checked, while the finding `Files` field still references the real repo-relative file path.
   - Compare the same exact coverage-unit set with every batch's `Implementation Inventory`. Require at least one row per unit, permit repeated units for separate responsibilities and grouped same-outcome internal definitions, and reject missing units or high-confidence named definitions, duplicate or generic `Contract ID`s, extra units, fabricated or source-unanchored basis/discovery rows, trace/result contradictions, missing or duplicate evidence type/counterfactual/invariance declarations, source-only PASS for stateful/external claims, `PASS` without concrete verification, and any non-atomic result/finding mapping. Every `GAP`/`BLOCKED` row has exactly one file-bound finding, and every finding cites exactly one `GAP`/`BLOCKED` Contract ID; a compound outcome must be reissued as separate rows/findings. A valid table proves that the manual trace was recorded, not that the judgment is automatically correct.
   - Confirm required journey reports exist at `reports/journey_audit.md` and `reports/visual_journey_audit.md` when interface-relevant files were queued, and that `effort_ledger.json` records those workers as completed or manual fallback.
   - Confirm each worker wrote one Markdown report under
     `<audit-output>/reports/` using the exact filename `batch_###.md`; verify
     the receipt's hash/byte claim before treating the worker as complete.
   - Inspect suspicious high-impact findings directly as lead before including them.
   - Reconcile every responsibility-level row by `Contract ID`. Reopen and recheck all cross-file contracts, high-risk paths, gaps, blocked or ambiguous evidence, and stateful/external PASS claims. For clean file-local contracts, preserve the verifier-bound worker anchors and result without duplicating the same source inspection. Do not infer end-to-end completeness by merely concatenating file-local rows.
   - Complete generated `<audit-output>/lead_reconciliation.md` and save the result as required `<audit-output>/reports/lead_reconciliation.md`. It contains `Run ID`, `Worker` with exact value `lead_reconciliation`, `Cross-File Contract Trace`, atomic `Findings`, and `Open Questions`. In `Cross-File Contract Trace`, assign every file-local, cross-file, public, or operational contract a unique sequential `lead:C###` ID; map every batch `Contract ID` to exactly one lead row; cite every mapped source and concrete evidence; and give evidence-backed `pass`, `gap`, `blocked`, or `not applicable` statuses for all nine implementation trace labels. The lead verification cell records its independent recheck with exactly one evidence type and counterfactual or invariance; source-only evidence cannot close a stateful or external PASS. Derive the overall `Result` from those statuses and never hide a mapped batch `GAP` or `BLOCKED`. Use the exact `No findings.` sentinel when clean. The verifier must validate this manifest-declared artifact; a final response or private lead notes cannot substitute for it.
   - When visual checks are applicable, populate `<audit-output>/visual_evidence.json`. Reports must cite stable `evidence:<id>` values. Each screenshot/native snapshot must be a real confined file with verified SHA-256, MIME, dimensions, route, state, viewport, and capture tool. Web audits bind the actual journey-aware formal-verifier JSON, initial/full-page screenshot pairs, changed-review queue, and finalized manual-review manifest, including checked-page coverage, visible scrollbars, palette risks, pending cells, and carried gaps. Filenames or words such as “screenshot” are not evidence.
   - Finish the formal verifier and every other automatic test before visual judgment. Open only cells in `review-queue.json`, never carried unchanged screenshots; finalize pass/gap/blocked decisions with `devcoordinator2-tooling formal-ui review`. Screenshot pixel/hash drift proves integrity only and never selects manual work.
   - Run the verifier only after all required batch, journey, visual, and lead-reconciliation reports are saved:
     ```bash
     devcoordinator2-tooling audit verify-full-repo \
       --manifest <audit-output>/manifest.json \
       --reports <audit-output>/reports \
       --receipt-out <audit-output>/verification_receipt.json
     ```
   - Treat verifier failures for unresolved scope warnings, incomplete effort or journey-worker ledger entries, queue marker mismatches, missing or invalid implementation inventories or lead reconciliation, missing or boilerplate interface inventory rows, missing high-confidence visible controls/messages, generic interface traces, placeholder/stub source omissions, malformed or boilerplate finding fields, out-of-batch finding file references, missing files, `UNCHECKED` rows, missing/mismatched SHA-256 values, stale run ids, changed files, unavailable repo source for source-backed checks, excluded-file digest drift, or directory-only descriptions as blockers before final synthesis. Every `scope_warning: true` row is intentionally unresolved until it is requeued or cleared by regenerating the queue.
   - If a run is interrupted or `effort_ledger.json` claims a worker is complete but its report file is missing, treat the verifier as authoritative and recover from the manifest rather than stale effort records: inspect `manifest.json` and `reports/`, mark missing batch/journey/visual/lead-reconciliation reports back to pending or rerun them, save the exact report filenames, update `effort_ledger.json` with the new agent/fallback status, and rerun the verifier. Do not claim completion from effort-ledger status alone.
   - Requeue any source-like unknown file or source-backed UI asset needed to resolve a warning by regenerating the queue with `--include-file`, `--include-glob`, or `--include-assets`, then rerun the affected batch, lead reconciliation, and verifier.

6. **Synthesize the implementation plan**
   - For large repos, first consolidate findings mechanically instead of merging
     hundreds of reports by hand:
     ```bash
     CANONICAL_SKILL_ROOT="$(dirname "$(dirname "$(realpath "$FULL_REPO_AUDIT_SKILL_DIR")")")"
     devcoordinator2-tooling audit merge-findings \
       --reports <audit-output>/reports \
       --manifest <audit-output>/manifest.json \
       --markdown-out <audit-output>/consolidated-findings.md \
       --json-out <audit-output>/consolidated-findings.json
     ```
     The manifest restricts input to the exact verified report allowlist and
     hashes. The merger deduplicates only findings whose immutable fields all
     match, ranks them P0→P3, and records which reports raised each one. Use it
     as the starting point for lead synthesis; it does not replace lead judgment.
   - Deduplicate batch findings.
   - Separate confirmed issues from hypotheses and open questions.
   - Directly inspect every consolidated candidate as lead, not only high-priority candidates. Assign exactly one disposition: `confirmed`, `duplicate`, `hypothesis`, `invalid`, or `out_of_scope`, with a concrete reason. Findings must be atomic before projection: if a worker or lead finding contains multiple independently completable obligations, reject and reissue the owning report with separate atomic findings, refresh lead reconciliation, rerun verification and consolidation, and only then project. Combine true duplicates through disposition; never simulate a split by editing or adding projection candidates.
   - Prioritize by user impact, correctness, security/reliability, blast radius, and implementation dependency order.
   - Produce an implementation plan with concrete verification steps that reproduce or demonstrate each gap across the intended product contract.

7. **Import confirmed findings into the database completion ledger**
   - After `devcoordinator2-tooling audit verify-full-repo` passes, create the consolidated findings and a lead-review projection outside the audited repo:
     ```bash
     CANONICAL_SKILL_ROOT="$(dirname "$(dirname "$(realpath "$FULL_REPO_AUDIT_SKILL_DIR")")")"
     devcoordinator2-tooling audit merge-findings \
       --reports <audit-output>/reports \
       --manifest <audit-output>/manifest.json \
       --markdown-out <audit-output>/consolidated-findings.md \
       --json-out <audit-output>/consolidated-findings.json \
       --ledger-projection-out <audit-output>/completion_ledger_projection.json
     ```
   - Complete every projection disposition. After every candidate is disposed, set top-level `review_status` to `complete`, including when the candidate list is empty. Only `confirmed` unresolved implementation, verification, integration, limitation, improvement, or generalization obligations become database issues. Keep hypotheses, open questions, audit-environment limitations, exclusions, invalid findings, and resolved work outside the active projection. A concrete external blocker uses state `blocked` and names its unblock condition.
   - Map each confirmed atomic obligation to exactly one issue with stable ID, plain-language remaining outcome, user or product impact, active state, verification requirement, priority, and affected repo-relative paths. Raw evidence and audit chronology stay in the external artifacts.
   - Preserve every unrelated database issue and permanent event. Reuse an existing ID only by marking the candidate `duplicate` of that issue. Never remove an issue because a later audit did not rediscover it, never prune during import, and emit an empty import for a clean audit. Later implementation and verification append transitions; they never delete the issue.
   - Keep the completed exact projection in the audit output. When ledger mutation is authorized, record each confirmed projection issue in DevCoordinator2's completion ledger with `task_create` (equivalently `devcoordinator2 call task.create`): the plain-language remaining outcome as `title`/`outcome`, the user or product impact as `impact`, the verification requirement as `verification`, affected paths and the projection's stable issue ID in `technical_note`, kind `stub` or `improvement`, sized in estimated lines of code. Before recording, search the existing ledger (`plan_overview`, `task_history`) for the stable ID in `technical_note` and update that task instead of duplicating it. A `daemon_unavailable` or database error keeps the import not applied; report the blocker. Recording a finding is not fixing it and does not make the audited product ready.

## Subagent Artifact Requirements

Require each batch subagent's saved report to contain:

- `Run ID`: the exact run id from the batch prompt.
- `Batch ID`: the exact `batch_###` id.
- `Batch Summary`: what the files collectively do.
- `File Coverage`: one row per listed file with `CHECKED` or `UNCHECKED`, the exact SHA-256 from the batch prompt, and a one-line purpose.
- `Implementation Inventory`: at least one row per owned file or range unit
  and one row per distinct contract responsibility, using the exact columns
  `File/unit`, `Contract ID`,
  `Contract/responsibility`, `Entrypoints/source anchors`,
  `Implementation/data/side-effect trace`,
  `Failure/edge/permission/recovery trace`, `Verification evidence`, and
  `Result`. Repeating a unit for separate responsibilities is required when
  applicable. Same-outcome internal definitions may share a row; each named
  definition must still appear in that row's anchors,
  implementation trace, and verification evidence rather than being listed as
  an untraced token. Use globally unique, stable IDs `batch_###:C###`, numbered
  within the owning batch; cross-file lead-reconciliation rows use
  `lead:C###`. Cite concrete backticked source anchors or the assigned byte-unit
  hash for a non-text byte range and show an ordered entry-to-outcome trace.
  Begin the implementation, failure/edge/permission/recovery, and verification
  cells with `pass`, `gap`, `blocked`, or `not applicable`; derive `Result` as
  `PASS`, `GAP`, or `BLOCKED` from those statuses. A `PASS` requires concrete
  evidence that the intended outcome, side effects, and applicable failure path
  were actually verified; source existence, type/shape assertions, or generic
  prose are insufficient. Every `GAP`/`BLOCKED` row maps one-to-one to an
  independently closable file-bound finding that cites exactly its `Contract
  ID`; ranged findings also cite the exact unit ID.
  In every responsibility cell, append exactly one `Basis: <kind> —
  <backticked reference>` using `user-requirement`, `acceptance-criterion`,
  `recorded-decision`, `public-contract`, `interface-promise`,
  `caller-contract`, `schema-invariant`, `operational-contract`, or
  `source-inferred`, plus exactly one `Discovery: parsed|manual — <backticked
  assigned-unit anchor>`. Parsed discovery uses a recognized named definition;
  otherwise manually enumerate the responsibility or mark it `BLOCKED` when
  enumeration is unavailable. Every verification cell declares exactly one
  `evidence-type: test|runtime|source-only` and one concrete `counterfactual:
  ...` or `invariance: ...`. Stateful, integration, external-effect, and
  success PASS rows require test or runtime evidence.
- `Interface Inventory`: for every interface-relevant file, one or more table rows with file, surface, visible label/control/message, expected behavior path, and actual implementation notes; visible text must be concrete source text or an explicit `None found` note, not boilerplate; for non-interface batches, the exact sentinel `No interface-relevant files in this batch.`
- `Findings`: prioritized, independently closable issues with file references, concrete evidence, expected behavior or standard, gap, and suggested fix direction. Each block cites exactly one `GAP`/`BLOCKED` Contract ID and maps to exactly that row. Finding `Files` fields must reference only files owned by the batch.
- `No Finding Notes`: files checked with no notable issue.
- `Open Questions`: ambiguity the lead should resolve.

Reject or requeue a batch result if it omits files, claims broad conclusions without file-level inspection, or proposes edits without evidence.

Require the journey source worker to return `Run ID`, `Worker`, `Journey Sources`, `Proposed Journeys`, `UI Source Journey Checks`, `Findings`, and `Open Questions`. Require `UI Source Journey Checks` to cover every confirmed and drafted journey and to compare current UI hierarchy against journey-derived relevance: critical information, primary navigation, secondary information, rare/detail/debug content, interaction target expectations, message metadata relevance, UI assumption status, and desktop/native/mobile fit. Require the visual journey worker to return `Run ID`, `Worker`, `Visual Tooling`, `Visual Journey Checks`, `Changed Visual Review`, `Findings`, and `Open Questions`. `Changed Visual Review` binds queued initial/full-page pairs and the formal report/queue/manual-review chain; carried rows explicitly say their images were not reopened, and carried gaps remain findings. Journey and visual workers must include the interaction checklist labels `badge-detail`, `row-hit-target`, `navigation-cursor`, `transient-disclosure`, `disclosure-scrollbar`, `icon-meaning`, `stable-expansion-width`, `hover-copy`, `status-summary`, and `message-metadata` in `UI Source Journey Checks`, `Visual Journey Checks`, or the first line of `Findings`; any `gap`/`blocked` status must have a finding. Their `Findings` sections must use the same field schema as batch findings (`Files`, `Evidence`, `Interface evidence`, `Expected behavior/standard`, `Gap`, `Suggested direction`) or the exact clean sentinel `No findings.` If no explicit journey documentation exists, the journey source report must include draft journeys marked `draft-needs-user-confirmation`, and the final audit must either ask the user to confirm them or label UI/journey coverage as assumption-based.

## Final Audit Artifact And Chat Output

Write the complete plan to `<audit-output>/final-report.md` with exactly these
top-level Markdown headings, in this order:

## Coverage
Repo root, audit output path, run id, source files queued, coverage units queued, files or units checked by subagents or manual fallback, implementation contract items inventoried, trace counts by pass/gap/blocked/not-applicable, files rechecked by the lead, journey workers run, scope warnings, exclusions, unchecked files or units, verifier result, subagent capability check result, ledger-recorded lead effort status, and per-batch/journey effort/fallback ledger.
State effort as `runtime-attested`, `ledger-recorded-unverified`, or `manual-fallback`; never shorten an unattested claim to “confirmed x-high.” List every high-risk file and lead-review status. List every bound visual evidence id/path/hash/viewport and formal-verifier evidence id.

## Architecture Findings
Cross-cutting structural design, ownership/boundary, dependency, lifecycle, and product-flow risks from the lead pass, or `No confirmed findings.` Put contract-to-outcome feature completeness in `## Implementation Findings`; cross-reference instead of duplicating the same issue.

## Implementation Findings
Include the verified lead trace table, showing each `lead:C###` ID and every
mapped batch `Contract ID` exactly once, with statuses for
`entry-registration`, `core-logic`, `data-lifecycle`,
`integration-boundary`, `authorization-trust`, `failure-recovery`,
`observable-outcome`, `operational-lifecycle`, and `verification`. Derive each
lead result from those statuses and preserve every mapped batch `GAP` or
`BLOCKED`, then describe confirmed semantic implementation gaps. Identify
hard-coded substitutes, ignored inputs, incomplete plumbing, false
success/persistence, missing registration or lifecycle work, production
mocks/fixtures, and shallow verification even when no explicit marker exists.
When every item is accounted for and no gap remains, keep the table and follow
it with `No confirmed findings.`

## Interface Findings
Buttons, menu items, links, form fields, messages, command items, empty states, toasts, or labels that point to missing, unimplemented, partial, untested, misleading, wrongly wired, wrongly prioritized, or journey-mismatched behavior, or `No confirmed findings.`

## User Journey Findings
Missing or unclear journeys, explicit UI assumption status (`confirmed`, `source-inferred`, or `missing`), unconfirmed drafted journeys, navigation relevance problems, information hierarchy mismatches, missing decision information, overexposed rare/detail/debug content, hidden critical information, desktop/native/mobile compactness or visibility/cropping/readability issues, missing test mode, unavailable visual test tooling, or `No confirmed findings.`

## File-Level Findings
Confirmed issues from batch results, deduplicated and grouped by feature area, or `No confirmed findings.`

## Completion Ledger
State whether database mutation was authorized or required by applicable project instructions. Always give the reviewed projection path/hash and confirmed/excluded/deduplicated counts. Give the database-import path/hash, stable import ID, candidate-to-issue mapping, imported/deduplicated issue IDs, and import result only when generated or applied; otherwise mark each as `not generated` or `not applicable`. If mutation was not authorized or the database interface was unavailable, say `Not applied—exact reviewed database import retained outside the repository.` If there are no confirmed findings, say that the empty import created or removed no issue.

## Implementation Plan
Ordered work items with expected behavior, affected files or modules, implementation steps, tests, risk, and completion-issue IDs when applied. Use `No implementation work recommended from this audit.` when clean.

## Verification Plan
Commands, browser/API checks, fixtures, or user workflows needed to prove each improvement. Include the exact verifier command and any commands already run.

Do not paste this full report, trace table, worker report, consolidated finding
set, or raw command log into chat. The final chat response contains only the
overall outcome, incomplete/blocking status, critical and total finding counts,
verifier result, material coverage caveats, and paths to `final-report.md` plus
the audit artifact directory. Keep it compact enough to serve as an artifact
index, not a second report.

Use this priority scale:

- `P0`: Security/data-loss/runtime failure or feature unusable for core workflows.
- `P1`: Major user expectation gap, partial feature, broken integration path, or standards gap likely to affect real use.
- `P2`: Important quality, maintainability, accessibility, performance, or test gap.
- `P3`: Polish, documentation, cleanup, or low-risk consistency improvement.

## Interface Audit Requirements

During both lead and subagent review, inspect UI implementation as a source of product promises and as a journey-specific information hierarchy. Inventory visible controls and copy, then check whether the code actually implements what a user would expect and whether the interface shows the right things at the right priority for each user journey.

Look for:

- Buttons, icon buttons, menu items, tabs, command palette items, links, and keyboard shortcuts with missing handlers, placeholder handlers, TODO handlers, console-only handlers, disabled dead ends, or handlers that do not persist/navigate/refresh.
- Text fields, filters, selectors, toggles, uploads, settings, and forms whose values are ignored, only locally mocked, not validated, not saved, or not reflected in UI/API behavior.
- Toasts, banners, empty states, tooltips, helper text, onboarding copy, success/error messages, and labels that promise capabilities not implemented in code.
- UI states that are missing or misleading: loading, empty, error, permission denied, offline, undo/redo, destructive confirmation, optimistic update rollback, or background job progress.
- Navigation surfaces that expose unavailable routes, hidden admin-only paths without permission handling, orphaned pages, or pages not linked from expected menus.
- Accessibility and interaction gaps that make an implemented feature behave as partially implemented: unlabeled controls, keyboard traps, focus loss, non-semantic buttons/links, or state conveyed only visually.
- Journey relevance gaps: most-probable routes hidden behind lower-probability routes, critical-always information buried under low-value content, primary actions displaced by rare actions, rare/detail/debug content permanently consuming prime screen space without journey rationale, lower-importance information lacking a clear access model, or source-inferred UI assumptions treated as confirmed product truth.
- Visual hierarchy gaps: cards or blocks nested inside other cards, repeated borders/background changes, inconsistent gutters, random-looking placement, disclosure controls that jump or change width, permanent obvious instructions, meaningless icons, decorative avatar clutter, hover affordances that clutter by default or vanish before they can be reached, or inconsistent message alignment that makes the journey harder to scan.
- Handoff conflicts: source-inferred or audit-generated UI assumptions, over-prescribed always-visible detail, duplicate severity/status/duration summaries, vague labels, source-model leakage, missing navigation destination/cursor rules, missing transient disclosure lifecycle, or detail controls dominating the default state.
- Compactness and responsive-fit gaps: desktop, native, or mobile screens where critical journey information does not fit, accidental horizontal scrolling, cropped/truncated decision data, overlapping content, hidden overflow without a scroll path, unreadable contrast, invisible theme text, large decorative or low-relevance areas that displace primary workflow content, or controls whose labels/states do not fit their containers.

For each interface finding, capture the visible label or message text, the journey(s) affected, the file that defines it, the expected implementation path, the actual implementation path, the current visible priority, the expected priority, and the missing, wrong, hidden, or overexposed behavior.

## User Journey Audit Requirements

Audit whether the repo clearly describes the most important user journey(s) through the UI. Accept journey docs, route maps, onboarding specs, product docs, tests, Storybook stories, analytics notes, support/common-task docs, or source-backed route/component flows. If none exist, draft likely journeys from the app intent and visible code, ask the user to confirm the most frequent use cases, and keep unconfirmed journeys as assumptions/open questions. Relevance is journey-relative: the same UI element can be critical for one journey and rare for another, so audit every confirmed or drafted journey independently before synthesizing cross-journey priorities.

During the source journey pass, check:

- Primary navigation and decision elements are visible, reachable, and visually prominent relative to less relevant elements. Estimate relevance as `critical-always`, `primary-frequent`, `secondary-occasional`, or `rare-under-5-percent`.
- Users have enough information on each screen, on desktop, native, and mobile surfaces, to make the documented journey decision. Rare or conditional information should remain reachable through an appropriate detail path; warning/threshold information must be available at the decision point.
- Important information is not cropped, hidden behind accidental overflow, unreadable, or displaced by lower-relevance content. Dense screens should mean compact decision support, not always-visible raw/detail/debug material or visual frame stacks.
- Most probable navigation routes for the journey are first, easiest, or most prominent; less probable routes are secondary, grouped, menued, or lower in the layout.
- Rarely needed detail has an appropriate access path when screen space is tight; critical or warning detail is never hidden from the decision point.
- Critical journey information and primary actions fit on desktop, native, and mobile surfaces without overlap, accidental horizontal scrolling, unreadable compression, low contrast, invisible theme text, or being buried under decorative or low-relevance content.
- Heavy or side-effecting UI actions can be exercised in a test mode, fixture mode, mock data mode, dry run, preview mode, or other safe visual path.

During the visual journey pass, use available visual tooling such as Playwright, Cypress, Storybook, browser MCP tools, native UI preview tools, or screenshots. Prefer test mode; use production mode only when the user explicitly requested it or the journey has no heavy/side-effecting operations. For web routes/states, map the primary journey, semantic initial-viewport regions, continuation, theme, and exact UI code/style/token/font/asset review inputs before running the formal verifier. Check desktop, native, and narrow mobile viewports for journey completion, real navigation rather than abstracted mocks, visible decision information, route priority, information hierarchy, rendered journey usefulness, theme consistency, palette risks, readable font sizes, sufficient color contrast, non-cropped/non-truncated content, scroll paths for overflow, unnecessary horizontal scrolling, nested visual frames, grid/alignment discipline, stable disclosure controls, icon meaning, instruction noise, and message alignment. When visual checks are applicable, include the command/tool used and the full bound evidence chain in the report. For CLI, library, plugin, or skill packages that expose only metadata/Markdown and no repo-owned rendered UI surface, mark visual checks as `not applicable` with evidence instead of treating host-owned rendering as a repo defect.

## Harness

`devcoordinator2-tooling audit build-full-repo` creates audit-run artifacts:

- `manifest.json`: source-file inventory, coverage-unit inventory, batch membership, and coverage invariants.
- `audit_index.md`: lead-agent instructions and a batch table.
- `.full-repo-audit-artifacts.json`: ownership marker that lets reruns clean stale harness artifacts without deleting user files.
- `queue_complete.json`: queue-generation marker written last with `phase: queue_generated` and `audit_verified: false`; if it is absent or the run id differs from `manifest.json`, regenerate the queue before dispatch.
- `effort_ledger.json`: lead-recorded capability, effort, fallback, and per-batch assignment ledger that must be completed before final synthesis.
- `visual_evidence.json`: real screenshot/native/formal-verifier, review-queue, and manual-review artifact bindings; generated empty and completed by the visual worker when visual checks apply.
- `journey_audit.md`: source-level user-journey worker prompt generated when interface-relevant files exist.
- `visual_journey_audit.md`: visual journey worker prompt generated when interface-relevant files exist.
- `lead_reconciliation.md`: generated lead prompt for cross-file contract tracing and atomic lead findings.
- `verification_receipt.json`: pass-only verifier receipt binding the manifest, exact report root, and SHA-256 of every authorized batch, journey, visual, and lead report; required by manifest-mode consolidation.
- `batch_###.md`: subagent-ready prompts with exact file or range ownership and interface-specific checks when relevant.
- `excluded_files.json`: files skipped with reasons for transparency.
- `reports/`: required destination for returned reports, including one exact `batch_###.md` file per batch and manifest-declared, verifier-checked `lead_reconciliation.md`.
- `consolidated-findings.json` / `.md`: candidates from the manifest's exact verified report allowlist and hashes; immutable finding fields are preserved and only exact matches deduplicate, but the merge does not confirm correctness.
- `completion_ledger_projection.json`: lead-reviewed disposition and exact active-row projection for every consolidated candidate; created after report verification, not during queue generation.
- `completion-ledger-database-import.json`: digest-bound transactional issue import produced only after audit/projection validation.

Companion Rust commands included with this skill:

- `devcoordinator2-tooling audit verify-full-repo`: result verifier for returned subagent `Run ID`, exact report section shape, `File Coverage` SHA-256 tables, per-unit and responsibility-level source-backed `Implementation Inventory` coverage with unique `Contract ID` and atomic result/finding linkage, required manifest-declared `lead_reconciliation.md` cross-file trace and atomic findings, `Interface Inventory` coverage and concrete visible-text checks, source-backed UI asset evidence, generic trace rejection, obvious placeholder/stub/dead-control source omission checks, finding severity/field shape/content and batch-file binding, current file fingerprints, queue marker consistency, effort and journey-worker ledger completion/provenance fields, `excluded_files.json` count/digest consistency, policy-blocking unresolved scope warnings, and a pass-only stable-input verification receipt.
- `devcoordinator2-tooling skills self-test audit-tooling`: deterministic Rust fixture tests for classification, interface detection, env/generated/vendor exclusion and opt-in behavior, batch invariants, evidence-contract enforcement, and result verification. These tests prove verifier behavior, not manual-agent semantic recall.
- `evals/marker-free/`: six isolated manual-agent evaluations for ignored input/config, partial plumbing, false persistence/success, missing registration/lifecycle, production fixture data, and shallow outcome tests. When supplied with responses from fresh agent runs, its deterministic scorer can measure finding recall and intentional-lookalike precision. Its self-test uses synthesized oracle-derived responses and proves only the schemas and scorer, so it is not empirical evidence of agent performance; keep actual run results separate from verifier self-tests and do not generalize one run into a mechanical guarantee.

The harness is the source of truth for batch coverage; the lead agent is responsible for validating that every manifest source file has a file-coverage row, every distinct responsibility has a unique `Contract ID` inventory row, and `reports/lead_reconciliation.md` proves cross-file semantic reconciliation before final synthesis. The verifier can enforce evidence shape and source binding; it cannot replace the lead's judgment about whether arbitrary domain behavior is correct.
