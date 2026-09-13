# Review and delivery evidence API

These protocol-v2 operations are passive Coordinator services. Codex owns
purpose changes, review jobs, delivery requests, deadline revisions, timers,
postponements and implementation admission. Coordinator does not schedule
agents, store a second usage ledger or change a clock when a review completes.

## Caller work context

An optional `client.work` is exactly this versioned snake_case envelope:

```json
{"version":1,"native_project_id":"native-clock-hash","thread_id":"thread-id","turn_id":"turn-id","operation_id":"operation-id","workstream_id":"implementation","outcome_id":"outcome-id","experiment_ref":"review-id@2"}
```

Only `version`, `native_project_id` and `thread_id` are required. The remaining
fields are optional/null. Version must be 1, required identifiers nonempty,
individual strings at most 256 UTF-8 bytes, and the complete envelope at most
2 KiB. Unknown fields/versions, control characters and malformed values are
not accepted as attribution. Identifiers only: no headers, commands, private
paths, credentials or content. `native_project_id` is the caller's clock key,
not the Coordinator repository ID; the server still resolves real repository,
run, source snapshot and caller UID independently.

The CLI reads this object automatically from `DEVCOORDINATOR_WORK_CONTEXT`.
It keeps `--client` unchanged and uses `thread_id` as the default client session.
An explicit matching `--session` retains attribution; a different explicit
session omits the work context instead of assigning the run to the wrong task.
The CLI sets adjacent `client.work_source` to `environment`. Direct protocol
metadata defaults to source `request`. Provenance is informational, not proof
of identity or authority. Malformed environment/metadata is omitted, with a
fixed `work_context_invalid`, `work_context_too_large` or
`work_context_session_conflict` diagnostic. Neither diagnostics nor warnings
echo the raw value, and attribution failure never cancels an otherwise valid
command. Request identity assertions still require the kernel-authenticated
edge UID; context never grants access, redirects a repository or changes argv.

Run summaries, history and retained run evidence store optional
`work: {context, source, diagnostic}` with the exact accepted context. Unknown
attribution has `context:null` and a diagnostic when supplied metadata failed;
old records omit `work`. Active, terminal and interrupted/recovered receipts
preserve the original attribution. Existing count/2-MiB receipt bounds retain
whole newest records rather than truncate identifiers. The existing trusted
local/administrator run-reading boundary is unchanged.
Context-bearing `test.history` pages also stay within a 12-KiB row-body budget
and return the last included run ID in `next_before` for forward progress.

`review.prepare` exposes these run cross-references as `evidence[].work`, beside
the real run reference, source/configuration identities and measured run timing.
Evidence pages also retain a 12-KiB body budget and return `next_offset` rather
than truncate a context. Missing work context is explicit coverage. A supplied
operation/outcome/experiment ID is a join key, not a fabricated allocation of
repository-wide tokens or elapsed time. Canonical task-usage gaps remain until
the accounting source actually supplies that attribution.

The canonical usage reader accepts exactly database versions 4, 5 and 6 with
taxonomy 1. Reviewed migration `0006_work_bindings.sql` only adds an append-only
binding table and its indexes; existing measurement tables and their queries
are unchanged. Unknown later versions remain unavailable, never zero usage.
These bindings remain in the canonical collector; Coordinator adds no usage
mirror, timer or scheduler.

## Parent integration: exact read-only receipts

`devcoordinator2 review show RECORD_ID@REVISION --format json` calls
`review.receipt` / MCP `review_receipt` with `{"reference":"RECORD_ID@REVISION"}`.
It returns the exact permanent revision, never an alias for the newest revision.
Require envelope `ok: true`, `data.completed: true`, and matching
`data.record.repositoryId`, `data.record.workstreamId` and review window
`data.record.windowStartMs` / `windowEndMs`. `data.reference` is the immutable
reference returned by `review.record`; do not substitute a decision ID or a
caller-written statement. `data.record.projectId` equals `repositoryId`.

`devcoordinator2 release evidence RECEIPT_ID --format json` calls
`release.evidence` / MCP `release_evidence` with `{"reference":"RECEIPT_ID"}`.
Require envelope `ok: true`, `data.qualified: true`, matching
`data.repository_id`, `data.target`, and expected `data.source_sha256`.
`data.verified_at_ms` is the actual retained delivery observation timestamp;
it equals `data.delivered_at_ms`, not the later receipt/lookup time.
`data.checked_at_ms` is when Coordinator checked the retained evidence.
Pending receipts have `qualified: false`, `verified_at_ms: null` and
`delivered_at_ms: null`. They cannot reset a delivery clock. The full
`qualification` enum is `qualified | pending_external_evidence`.

These lookups use only authority-database receipts and do not probe the network,
recompute usage, materialize artifacts or require an installed daemon self-test.
The usual live client uses the configured socket; isolated tests use a candidate
control plane and disposable database. Missing references return typed errors.
Previously qualified evidence describes an actual past verification, not a
claim that an external URL or a retention-limited artifact is available forever.

### Web deployment observations

`release.deliver_evidence` also accepts `kind: "web-deployment"`. A website is
not represented as a registry package or downloaded application artifact. Its
passing governed route check retains the actual returned HTML and a version-1
verification file with `observation: "web_route_passed"`. `file` identifies that
HTML inside the retained artifact; `observed_sha256` hashes the complete returned
body. `checked_at_ms` is the actual response observation time inside the passing
run, never the later receipt creation time. Checks must exercise the intended
application route and assert its expected content, not substitute a login or
health page. Normal rendered-interaction requirements remain applicable.

The verification file adds `deployment: {deployment_id, generation_number,
http_status, content_type}`. Qualification requires HTTP 200 HTML, a current
running generation in the same repository and an observation after that
generation was created. Coordinator recomputes the applied deployment fingerprint
from its recorded specification, commit/dirty state and the retained run's source
digest. Different source or configuration, another generation, a stopped
deployment and foreign ownership cannot qualify.

`access` is the actual observed route: either the deployment's owned HTTPS origin
or its exact assigned HTTP loopback port. An existing protected public-domain
route remains protected when a trusted local check uses loopback; qualification
does not publish a private deployment or alter access policy. Other origins,
ports, credentials, query strings and fragments are refused. The service validates
retained evidence and owned routing metadata; it does not perform network requests.

Use the existing `release deliver-evidence --file request.json` operation with a
new planned/requested release and the real retained catalog identities. Its
qualified receipt is then read by `release evidence RECEIPT_ID`, including by
native delivery clocks. A legacy `release deliver` record is not silently
upgraded; new qualification requires retained verification evidence. Existing
artifact, registry-package and local-executable contracts are unchanged.

## Prepare

`review.prepare` / `review_prepare` accepts:

```json
{"repository_id":"project-alpha","workstream_id":"spec","window_start_ms":1000000,"window_end_ms":605800000,"offset":0,"limit":10,"before_decision_seq":null}
```

CLI: `review prepare --repository-id ID --window-start-ms MS --window-end-ms MS`
with optional `--workstream-id`, `--offset`, `--limit`, `--before-decision-seq`.
The window is half-open, nonempty, within nonnegative signed-64-bit Unix
milliseconds, and never in the future. There is no calendar-duration cap:
coalesced offline/specification gaps keep the entire original start/end pair.
The existing canonical indexed usage reader supplies the exact window, not a
rounded dashboard range or stale display cache. Its counts, time dimensions,
activity, provenance, source schema/taxonomy coverage and explicit gaps are
returned without copying them into persistent tables. `source_refs` names the
canonical combined repository/window source; contributing account identities,
private collector paths and raw model/tool contents remain undisclosed.

Review usage reads share a 15-second budget across already-indexed collectors.
They reuse existing repository links rather than launch identity probes or
retry missing mappings. The source reader interrupts expired SQLite queries;
exhausted sources are reported as `query_budget_exhausted` and missing links as
`mapping_pending` in `usage.coverage.unavailable_reasons`. Completed source
measurements remain available with partial coverage; incomplete sources are
not presented as full totals or fabricated zeroes. The exact review window
is never shortened. Prepare/record CLI replies have a bounded 20-second wait
to receive that result; cheap receipt lookups retain their ordinary deadline.
Records must acknowledge these coverage gaps; no slicing or repeated model
requests are required merely because an unchanged inactive interval is long.

The bounded evidence page includes existing outcomes and retained run identities
overlapping the window, plus standing decision excerpts and supersession links.
Run entries expose actual `source_sha256` and `config_sha256` when their retained
evidence still exists; missing bindings are null and explicitly listed as gaps.
`next_offset` advances the evidence page; `next_before_decision_seq` independently
advances decisions. Current outcome state is contextual, not reconstructed
historical task state. Follow `task.history` and `decision.tail/search` for full
acceptance/rationale. Source references for runs are `WORKTREE_ID/RUN_ID`.
Workstream/task usage attribution and user-wait measurements are explicitly
unavailable in this reader; repository totals must never be allocated to them.
Retained history can expire and reads disclose bounded/truncated coverage.
User waits and intentional repeated release validation are not inferred waste.

## Record and revisions

`review.record` / `review_record` accepts
`{record_id?: string, expected_revision: integer, record: ReviewRecord}`.
CLI: `review record --file record.json --expected-revision 0` creates a record;
add `--record-id ID --expected-revision N` to append revision N+1. All writes
retain prior revisions. A stale revision fails atomically. Repository,
workstream and window identity cannot change across revisions.

`ReviewRecord` uses camelCase: `version` (1), `repositoryId`, `projectId` (same
Coordinator repository), optional `workstreamId`, `windowStartMs`, `windowEndMs`,
optional `outcomeId`, and `experiment` (`OptimizationExperiment`). The experiment
contains `hypothesis`, `evidenceRefs`, `alternatives` (2–5 distinct choices),
`chosenAction`, `baseline`, `successCriteria`, `rollbackCondition`, `disposition`,
`resultEvidenceRefs`, `scopeRepoId`, `preservesQuality`, `reason`, `observations`,
and optional `comparison: {narrative, inputChangeReason?}`.

`baseline` contains `evidenceRefs`, `interpretation` and `missingMeasurements`.
It references canonical measurements instead of persisting their numeric totals.
Every current prepare gap must be acknowledged. An evidence reference is
`{kind, reference}` with kind `usage | outcome | decision | run | release`.
Outcome/decision/run/release references must resolve in this repository. Baseline
usage references must exactly match the reviewed repository and window. Result
usage references are `canonical-usage:REPOSITORY_ID:START_MS:END_MS`, with
`START_MS >= windowEndMs`, `END_MS > START_MS` and `END_MS <= recorded_at_ms`.
They are read from canonical indexed sources, not accepted as supplied numbers.
Baseline and all distinct result windows share the same 15-second query budget.
General observations may cite either the baseline or declared result usage refs.
Decision
references accept either permanent decision IDs or their stable refs.

Retained/reverted interpretations require actual measured before/after sources,
a substantive `comparison.narrative`, and a passing result run or qualified
delivery. Missing measurements require `inconclusive`, not an improvement claim.
Result run finish times and delivery verification times must fall between
`windowEndMs` and record time, inclusively. Baseline quality references cannot
postdate the baseline window. These are actual stored timestamps, not run-ID
dates. Result coverage gaps use `result_usage_partial_or_unavailable`,
`result_total_tokens_unavailable`, `result_usage_query_budget_exhausted` and
`result_active_time_partial` where applicable; acknowledge them in
`baseline.missingMeasurements`.

When retained input identities exist for both sides, their test/workload or
delivery target must correspond. Source/configuration changes require
`comparison.inputChangeReason`; different candidates are not automatically
invalid. Missing input bindings require the explicit
`comparison_input_identity_unavailable` gap. The narrative must explain the
comparison, missing coverage and comparable work; the service verifies evidence
and boundaries, not machine-proved causality or arbitrary narrative assertions.
Intentional repeated validation of identical frozen inputs remains valid;
there is no comparison with mutable checkout HEAD and no mandatory applied
revision before one evidence-backed retained/reverted record. Legacy records
without the new comparison basis are not completed retained/reverted receipts;
append a properly validated revision rather than rewriting their history.

Each observation is `{kind, interpretation, evidenceRefs}`. Kinds are
`user_wait | intentional_validation | avoidable_work | other`. A completed
review needs substantive interpretation of non-total outcome/run/release
evidence and a standing decision, not just token totals. Missing measurements
may support an explicitly inconclusive or unchanged review, never invented
measurements. Applied/retained actions need evidence of avoidable work, not
only user waiting or intentional validation. `scopeRepoId` must equal the
reviewed repository and `preservesQuality` must be true. This is a validated
review contract, not an execution engine or proof that arbitrary proposed code
preserves quality; actual result verification remains necessary.

Dispositions: `proposed | applied | retained | reverted | inconclusive | unchanged`.
`proposed` and `applied` have `completed:false`; the other dispositions have
`completed:true` after validation. Retained/reverted actions require result
run/release evidence. Unchanged requires a specific reason and observations;
"no change needed" alone fails. Review completion never marks an outcome or
release delivered. Text/reference counts are bounded; each record is at most
8 KiB. `review.show` / `review_show` takes
`{repository_id, record_id?, offset:0, limit:10}`; CLI
`review list --repository-id ID [--record-id ID]`. Pages are at most ten
revisions and 24 KiB of record bodies, ordered append-first, with `next_offset`.

## Delivery evidence

The existing `release.deliver` / `release_deliver` deployment request and reply
remain unchanged. New `release.deliver_evidence` / `release_deliver_evidence`
and CLI `release deliver-evidence --file delivery.json` take:

```json
{"release_id":"RELEASE_ID","path":"/absolute/registered/worktree","run_id":"RUN_ID","check":"build","artifact":"package","manifest_sha256":"64-hex-digest","source_sha256":"64-hex-digest","target":"linux-cli","kind":"local-executable","verification_file":"delivery.json"}
```

Kinds: `artifact | registry-package | local-executable | web-deployment`.
A retained artifact tree is selected through the existing hash-bound artifact service. Repository,
manifest, source digest, finished passing run, tree and file hashes must match.
Focused development validation is sufficient for a preliminary delivery; this
does not promote it to complete release/readiness proof.

Without `verification_file`, the receipt remains pending and the release's
status/timestamps are untouched. Qualification requires a verification JSON
file retained by that passing check, not a report supplied inline by a client.
The report is capped at 8 KiB, denies unknown fields, and contains:

```json
{"version":1,"kind":"local-executable","target":"linux-cli","source_sha256":"64-hex-digest","file":"cli","observed_sha256":"SHA256_OF_CLI","checked_at_ms":692199900,"access":"artifact://WORKTREE_ID/RUN_ID/build/package/cli","observation":"executable_smoke_passed"}
```

`file` names another hash-verified file in the selected retained tree.
`observed_sha256` must match that file. `checked_at_ms` must fall inside the
retained run's actual start/finish interval. Kind, target and source must match
the request. A local executable must name the exact retained file in its
`artifact://` access reference; use the existing artifact catalogue/file or
materialization CLI to retrieve it, then restore executable permission before
launching the verified bytes. Artifact and registry-package observations are
respectively `download_matched` and `registry_download_matched` and require a
credential-free HTTPS access URL without query or fragment.

For a verified website, select `kind: "web-deployment"` in the same delivery
request and retain a report like this with the observed response body:

```json
{"version":1,"kind":"web-deployment","target":"web-preview","source_sha256":"SOURCE_DIGEST_FROM_RUN","file":"response.html","observed_sha256":"SHA256_OF_RESPONSE_HTML","checked_at_ms":1789185600123,"access":"https://preview.example.test/journey","observation":"web_route_passed","deployment":{"deployment_id":"DEPLOYMENT_ID","generation_number":3,"http_status":200,"content_type":"text/html; charset=utf-8"}}
```

The identifiers, digests, generation and timestamp above are illustrative;
the passing check must record the real values. The response body and report
belong to the same retained tree. See [Web deployment observations](#web-deployment-observations)
for the source, owned-route and observation requirements. Retaining website
evidence does not turn the website into a downloadable package.

The governed check owns the actual web-route, download or executable smoke
validation and must report its real observation. Coordinator verifies provenance, bindings and
integrity; it does not independently claim to have probed an external registry
or execute repository binaries as root. No inline `verified`/`qualified` flag is
accepted. Without that retained observation, external evidence stays pending.
Qualified receipts retain exact source, target, run, tree, manifest, metadata,
verification-file hashes and access. Re-submitting identical evidence returns
the same receipt and original timestamps; new evidence cannot redeliver an
already delivered release. Receipts survive retained-tree expiry as compact
historical evidence, not a new usage/artifact mirror.

`release.evidence_show` / `release_evidence_show` takes
`{release_id, offset:0, limit:10}`; CLI `release evidence-show RELEASE_ID`.
Use its returned receipt ID with the exact cheap lookup described first.

All new operations retain trusted-local/administrator authorization, governed
artifact no-follow/integrity controls and read-only canonical usage access.
Applicable confirmed assumptions: `DC2-2026-08-29-CODEX-USAGE-SOURCE`,
`DC2-2026-09-04-DAEMON-USAGE-SNAPSHOT-CACHE`,
`DC2-2026-09-03-RETAINED-EVIDENCE-TREES` and
`DC2-2026-09-05-FAILED-CHECK-EVIDENCE`. No new listener, writer, scheduler,
cross-account identity disclosure or external verification authority is added.
