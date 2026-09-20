# Command Contract

Envelope and error model: `protocol.md`. Full schemas below exist for the
operations in the exhaustive Rust registry. Every listed operation has a
typed implementation; the generated contract bundle is authoritative for
input/output schemas, policies, CLI routes, and MCP exposure.

The CLI maps `devcoordinator2 test start …` to operation `test.start`; the MCP
server exposes applicable operations as tools such as `test_start`, `test_retry`,
`test_status`, `test_log_catalog`, `test_log_tail`, `test_log_search`,
`test_log_range`, `test_log_failure_context`, `test_log_retention_get`,
`test_log_retention_set`, `test_stop`, `test_capacity_get`,
`test_capacity_set`, and `repository_list`. All three
surfaces return the identical result JSON. `event_wait` is the blocking
multi-filter event/heartbeat tool described below.

## ping

Args: none.
Result: `{"daemon_version": "<semver>", "schema_version": 16, "socket": "<path>"}`

## event.wait

CLI: `event wait`; MCP: `event_wait`. The operation blocks until at least one
authorized retained/new event matches, or one or more filter deadlines become
due. One request accepts 1..32 filters and returns at most 100 events.

Each filter has a unique `filter_id`, optional category list
(`test|deployment|planning|health|feedback|other`), optional exact event kinds,
optional repository/deployment allowlists, and optional RFC 3339
`deadline_at`. The request carries one optional monotonic `cursor` and a
`limit`. Omitting the cursor captures the journal head immediately before
registration, so publication cannot fall into a check/subscribe gap. Passing a
cursor replays retained events after it, including an event that arrived before
the wait request. A cursor outside retained history returns `cursor_stale`;
only trusted local or administrator callers receive bounded floor/head
diagnostics.

The result is `{cursor,events,heartbeat_due}`. Each event appears
once with every matching `filter_id`; a filter satisfied by an event does not
also receive a heartbeat. When the shared scheduler wakes, every due unsatisfied
filter for that client is returned together. Events have category-specific,
redacted payloads and global increasing cursors; the journal retains the newest
1,024 entries; public callers receive no global activity counts.
Disconnect and MCP cancellation remove the wait. The daemon has
one scheduler for all waits, not a timer or polling loop per subscription.

Native test, deployment, planning, feedback, repository, access, bug, and
startup changes publish directly. Health changes come from the existing single
host sampler/alert observer, which is the centralized polling boundary for
owned sources without a native event mechanism. No agent, conversation, turn,
task-execution, or wake-up semantics are introduced; clients decide what an
event or elapsed heartbeat means.

## test.start

Args:
- `path` (string, required) — any path inside the target worktree.
- `test` (string, optional) — named test from `.devcoordinator.toml`;
  defaults to the file's declared default.
- `checks` (array of unique check names, optional) — diagnostic selection plus
  its transitive prerequisites. Omit for the complete graph.
- `tier` (`development|pre-merge|release`, optional; default `release`) — the
  cumulative validation tier to run.

Omitting `checks` produces proof `complete`; supplying it produces proof
`selected` and always sets `readiness_eligible` false.

Console requests address an exact registered worktree. After the usual caller
authorization, repository code runs as its registered non-root account, so the
edge needs no Git or source-write access. Status retains the original `caller_uid`
and includes `execution_uid` only when the execution account differs. A client
cannot choose that account through request parameters. CLI/local-agent requests
retain their existing execution identity.

Result (only after the process exists):

```json
{
  "run_id": "t20260822T120000Z-1a2b3c",
  "repository_id": "r…", "worktree_id": "w…",
  "test": "unit",
  "requested_tier": "release",
  "readiness_eligible": true,
  "status": "running",
  "proof": "complete",
  "selection": [],
  "origin_run_id": null,
  "unit": "devcoordinator2-test-<worktree_id>-<suffix>.service",
  "summary_path": "<worktree>/.devcoordinator/test/current/summary.json"
}
```

Errors: `repository_not_found`, `repository_config_invalid`,
`worktree_busy`, `tests_draining`, `unit_stop_failed`, `test_start_failed`.
Never `queued`.
Latest-start-wins: a concurrent earlier run ends `superseded`. A replacing
start includes `superseded_run_id` with that exact active run; the field is
absent when no active run was replaced. Compose independent same-worktree
checks inside one schema-2 graph rather than starting competing named tests.

Additive `targets: ["api","ui"]` selects multiple declarations for one invocation,
instead of `test`. `checks` accepts qualified `target/check` names, and
`cases: {"target/check":["case-id"]}` selects exact cases. Responses retain
`targets` and `case_selection` through completion and retry; namespaced check
identities have a readable `display_name`. Run history preserves target names.

Every dependency-ready leaf enters host-wide adaptive admission immediately.
`after` requires terminal completion; `requires` requires success. Failed
preflights mark their declared targets `invalidated`; unrelated branches
continue. Process exit or an inherited exact completion event advances the
graph. A leaf `timeout_seconds` is a failure ceiling and produces `timed_out`;
the test-level value is the outer watchdog. Neither can produce success.

## test.retry

Args: `path`, optional `test`, `run_id` of an original completed full run, and
one failed `check`. Result has the same running shape as `test.start`, with
`proof: "retry"`, `selection: [check]`, and `origin_run_id` set. The
target's prerequisite closure runs; a matching process-completed prerequisite
with declared artifact receipts may be `reused`. Missing, unfinished,
diagnostic, non-failed, source/config-changed, or artifact-stale origins fail
before the current slot is changed. A retry never constitutes complete proof.

## test.status

Args: `path` (required).
Result: the current `summary.json` fields (see below) plus `summary_path`.
Successful runs carry **no log text**. Error: `test_not_found`.

Graph status additionally carries `requested_tier`, `readiness_eligible`, `proof`
(`complete|selected|retry`), `selection`,
`origin_run_id`, `check_summary`, ordered `checks` (state, monotonic duration,
exit status, declared artifact receipts, and logical log references), a
bounded structured `failure_index`, and `source_changed`. Each failure entry
contains check/case, exit code or signal, typed termination reason, normalized
source location when supplied, error category, bounded expected/actual values,
duplicate fingerprint/count, origin, and supporting logical log references.
It contains no raw output, stack, absolute path, or arbitrary reason prose. The
referenced Rust check report is schema 2 only; schema
1/Python reports are rejected rather than translated. It includes
`capacity {learned_capacity, effective_capacity, capacity_wait_count}` plus
bounded preflight/check/case counts. States are `pending|running|passed|failed|reused|`
`timed_out|invalidated|not_meaningful|cancelled|unsafe`. Capacity-wait counts,
expanded-case totals, and preflight totals remain bounded summary fields.

Checks may report `resource_waiting`, `phase`, and `phase_durations`.
Each phase reports summed execution `duration_seconds`, separately measured
`elapsed_seconds` (null when unavailable), and its node count. A database case
also includes `phases` for fixture preparation, case execution and cleanup.
Fixture and cleanup log selectors require both check and case identities.
Measurements never control success progression.

summary.json fields (result schema 2; schema 1 is not read or translated):
`schema_version`, `run_id`, `test`, `requested_tier`, `readiness_eligible`,
`proof`, `selection`, `origin_run_id`,
`status` (`running|passed|failed|timed-out|cancelled|interrupted|superseded`),
`started_at`, `finished_at` (null while running), `duration_seconds`,
`exit_code` (null unless exited), `stdout_bytes_observed`,
`stderr_bytes_observed`, `caller_uid`, `client`, `check_report_ref`, and
`log_catalog_ref`. Retained-byte and truncation compatibility fields are not
accepted; complete per-leaf metadata lives in the log catalogue.

## test.capacity.get | test.capacity.set

- `test.capacity.get` takes no arguments.
- `test.capacity.set` takes `{cap: integer|null}`. An integer sets the
  administrator maximum; `null` restores uncapped Auto admission. Lowering a
  cap never kills active work.

Both return `{learned_capacity, effective_capacity, cap, active, waiting,
paused, last_adjustment}`. `last_adjustment` is null or `{event_id, at, actor,
reason, previous_capacity, new_capacity, cap, p95_cpu_percent,
p95_memory_percent, saturation_fraction, epoch_seconds}`. Evidence values may
be null when unavailable. CLI equivalents are `test capacity show|set|clear`.

## test.log.catalog

Args: `path` plus optional `run_id` (current by default), `check`,
`phase` (`executor|check|discovery|fixture|case|cleanup`), `case`, `stream`, opaque `cursor`,
and `limit` (1..100). The result is a stable page of content-free entries:
logical `log_ref`, bytes, lines, first/last byte times, complete/truncated
state, SHA-256, age expiry, depth rank/limit, and structured-evidence
availability, plus `next_cursor`. It never returns log text or an absolute
path.

## test.log.tail | search | range | failure_context

All retrieval is bound to one authorized logical reference and one immutable
snapshot, with a complete encoded response below 64 KiB:

- `tail` requires `phase` and `stream`, defaults to 50 lines, and accepts
  `max_bytes` up to 48 KiB.
- `search` additionally requires literal `text`; it accepts bounded match and
  surrounding-line counts. Metacharacters are ordinary bytes, not a regular
  expression.
- `range` accepts exactly one inclusive line interval or zero-based half-open
  byte interval. Binary byte results use base64.
- `failure_context` requires one exact phase/stream selector, ranks recognized
  failure references for that stream deterministically, and returns bounded
  line-addressed excerpts; it never silently chooses another stream or invokes
  a language model.

Every content row carries one-based line numbers and zero-based byte offsets.
`next_cursor` continues without rereading; a changed or expired target returns
`cursor_stale` or `log_expired`. Returned text is explicitly untrusted test
output.

## test.log.retention.get | test.log.retention.set

`get` takes no arguments. `set` requires positive `max_age_seconds` and
`case_depth`; defaults are 86,400 seconds and three histories per logical case.
A completed leaf is removed when it exceeds either boundary. Changing the
setting wakes cleanup immediately; active runs are never removed. Both return
the stored settings and last bounded cleanup state. This requested
administrator action is immediate and has no second confirmation dialog.

An unlocked incomplete run left by interruption or supersession is retained as
truthful partial evidence (`complete: false`) and receives its own age/depth
bucket so it cannot evict sealed history. A malformed unrelated run never
denies an exact selected-run query; the selected run itself remains strict.

## test.artifact.catalog | test.artifact.file

`catalog` requires `path`, `run_id`, and `check`. Without `artifact` it returns
the bounded hash-bound tree summaries and run/source/config/proof identity.
With an exact artifact name it verifies that entire retained tree and returns a
stable file page using `offset` and `limit` (1..100); callers may bind later
pages with `manifest_sha256`. Results contain only declared artifact names and
relative file names, sizes, counts, and SHA-256 values—never a private source or
storage path.

`file` additionally requires `artifact`, relative `file`, and
`manifest_sha256`; `offset` and `max_bytes` select at most 180 KiB. The daemon
reopens every component without following links, verifies the complete file
against its manifest receipt, and returns
`{sha256,total_bytes,offset,bytes,base64,next_offset}`. Errors distinguish
expired, unknown, and changed evidence. CLI `test artifact materialize` pages
these two read-only operations, writes only a new caller-owned destination, and
revalidates every file and tree digest locally.

## test.evidence.get | test.evidence.image

`get` requires `path` and `run_id`. It returns one path-free projection of the
retained formal-UI bundles for that worktree/run: repository/worktree/run
identity, bundle check/phase/case and formal-run metadata, ordered cells,
available viewport/full-page screenshot identities, and linked screenshot
feedback. It returns no image bytes, selector, action value, entered value, or
absolute path. A retained run with no valid bundle returns
`status: "unavailable"` plus bounded content-free issue codes.

`image` requires `path`, `run_id`, and an `image_id` returned by `get`; optional
`offset` defaults to zero and `max_bytes` is 1..184320. The result is
`{image_id,mime,sha256,total_bytes,offset,bytes,base64,next_offset}`. Every
request revalidates the exact confined PNG and its complete SHA-256 before
returning one bounded chunk. Errors distinguish `test_evidence_expired`,
`test_evidence_not_found`, and `test_evidence_tampered`.

## test.evidence.feedback.*

All operations require `path` and `run_id` and are administrator-only.

- `create` requires one valid `image_id`, 3..2000 characters of plain `body`,
  and 1..64 normalized marks (`pin|rectangle|arrow|freehand|highlight|text`).
  It atomically creates a Plan `user_feedback` task and returns the thread.
- `reply` requires `feedback_id` and `body`.
- `edit` requires `feedback_id`, `comment_id`, and `body`; only that comment's
  author may edit it. Editing the root comment also updates the Plan task title
  and outcome.
- `state` requires `feedback_id` and `state: open|resolved`; the linked task
  becomes planned or done.
- `delete` requires `feedback_id`; only the annotation author may invoke the
  explicitly labelled destructive action. It drops the linked task but keeps
  permanent task and feedback event history.

## test.stop

Args: `path` (required), optional one-line `reason` (3..256 characters).
Result: `{"run_id", "status": "cancelled"}` after the cgroup is proven
empty, or `{"run_id", "status": "<terminal>", "already_finished": true}`.
Errors: `test_not_found`, `unit_stop_failed`.
The reason is stored as `termination_reason` operational metadata and never
changes cancellation into a test failure.

## repository.register

Args: `path` (required). Registration also happens implicitly on first
`test.start`.
Result: `{"repository_id", "worktree_id", "root_path", "worktree_path",
"display_name", "registered": true|false}` (`false` = already known).

## repository.list

Args: optional `include_archived` (boolean, default false).
Result: `{"repositories": [{"repository_id", "root_path", "display_name",
"registered_at", "last_seen_at", "archived_at", "archive_note",
"merged_into_repository_id", "worktrees": [{"worktree_id",
"worktree_path"}]}]}`. Normal collections exclude archived repositories.

## repository.archive / repository.unarchive

- `repository.archive {repository_id, merged_into_repository_id, note}` retires
  an inactive repository while preserving its permanent task and decision
  history. It refuses open tasks, elaboration requests, open releases, active
  deployments, or an active test.
- `repository.unarchive {repository_id, note}` restores an archived repository
  only while its original checkout exists.

Both transitions append `repository_events`. New mutations against an archived
repository return `repository_archived` and name its replacement; explicit
historical plan, task-history, and decision reads remain available.

## repository.status

Args: `path`.
Result: one repository object as above plus, when present, the current test
summary reference (`summary_path`, `status`).

## config.*

Existing server administrators and trusted local callers can use:

- `config.get {}` (CLI `config show`, MCP `config_get`) returns the configured
  policy's active/stored revisions, reload state, entry count, supported live
  and restart-required settings, validation state, and bounded value-free history.
- `config.env.set {path?, name?, deployment_id?, file, authorized,
  expected_revision}` (CLI `config authorize|revoke`, MCP `config_env_set`)
  changes one exact declared repository/file pair and activates it atomically.
  A grant validates the ignored regular non-symlink file; revoke can remove an
  obsolete entry without requiring the file to remain present.
- `config.reload {expected_revision}` validates and activates the already
  configured policy. Errors include `configuration_conflict`,
  `configuration_invalid`, and `configuration_restart_required`.

No operation reads environment values into a result, changes the policy
location, grants broader access, or restarts services.

## deployment.* (Phase 3, implemented)

Reference args on every command except `list`: `path` (required) plus
`name` (`web` or `web@checkout`) or `deployment_id`.

- `deployment.list {path?}` → `{deployments: [...], declared: [...]}` — all
  applied deployments; with `path`, also the declared-but-not-applied
  `name`/`source`/`deployment_id` triples of that repository.
- `deployment.preflight` → `{repository_id, name, ready, blockers:
  [{component, code, file, message}]}`. Deployment administrators can inspect
  every declared environment-file prerequisite without registration, port
  reservation, or runtime changes. `authorization_required` names the exact
  missing grant; preflight never supplies it automatically.
- `deployment.apply` → status (below) after: validate all environment-file
  prerequisites, fingerprint (spec + commit + dirty flag + source digest),
  reserve ports/domain transactionally, prepare the
  candidate (checkout worktree + optional build), start components in
  declared order (blue/green for generation-scoped components, in-place for
  stable data-owning ones), prove health, publish the route atomically,
  retire the previous generation in reverse order. Errors:
  `deployment_apply_failed` (detail = JSON of component states; candidate
  components are stopped and removed; nothing is routed), `busy`,
  `repository_config_invalid`. An identical running specification returns
  the status with `unchanged: true`.
  Native Compose applies validate every declared service. Finite services are
  recreated for a changed candidate, must exit 0, and produce bounded
  generation receipts; long-running services must be running/healthy. An
  instance-authorized ignored `env_file` is passed by validated path only.
- `deployment.status` → `{deployment_id, name, source, repository_id, state
  (running|stopped|degraded|applying|failed), current_generation,
  previous_generation, domain, route_port, ttl_expires_at, components:
  [{name, type, state, health, generation, binding{kind, identity}, port,
  restarts, owned, independent_control, last_error, services?: [{name, role
  (`finite|running`), state, desired_state, containers, independent}],
  completed_services?: [{service, generation, container_id, image_id,
  exit_code, started_at, finished_at, recorded_at}]}], log_dir}`.
  Managed status also returns `readiness: {ready, expected_components,
  missing_components, pending_apply, blockers}`. Missing declared components
  make aggregate state `degraded`. Explicit status/apply compare current source
  with the applied generation, including successive dirty edits; a running
  component is not proof that current code or a new migration has run. A null
  `pending_apply` means source freshness was not established (including control
  responses), never a readiness success. Lists report runtime state only;
  use explicit status for source freshness. Observed-only
  deployments have no managed-source readiness claim.
- `deployment.start | stop | restart {component?}` → status. Whole
  deployment in declared/reverse order, or one component whose declaration
  permits independent control. A declared independent Compose service is
  addressed as `<component>/<service>`; only exact containers carrying the
  recorded project+service identity are controlled, dependencies and finite
  setup are not started, and unrelated services/routes remain. Stop withdraws
  the route for a whole routed component, never for an unrelated Compose
  worker, and never deletes data.
- `deployment.logs {component, tail_lines<=5000}` → `{component, tail,
  log_path | container_id}`.
- `deployment.rollback` → status with `rolled_back_from`/`rolled_back_to`;
  checkout source only (`rollback_unavailable` otherwise).
- `deployment.apply` accepts an optional `candidate: {path, commit}` with an
  existing `deployment_id`. Trusted local callers can promote a clean linked
  worktree at the full expected commit into the same versioned deployment.
  Different repositories, changed/dirty candidates and live-source targets
  are refused before runtime mutation. The deployment keeps its identity,
  domain override, data ownership and generation history. The CLI exposes
  `--candidate-path` and `--candidate-commit` together with `--deployment-id`.
  Omitting `candidate` preserves the existing declared-source behavior.
- `deployment.remove {delete_data=false}` → `{removed, data_deleted,
  deleted_volumes}`. Stops everything, removes containers/units/generation
  checkouts and records; named volumes and PostgreSQL data survive unless
  `delete_data` is true. Repository `persistent_paths` are never touched.
- `deployment.set_domain {deployment_id, domain|null, port?, component?,
  public?}` (administrator) → `{deployment_id, domain, ...}`. Sets, changes,
  or clears the routed domain and republishes the route document. Managed:
  the label persists as an override that wins over the declared domain on
  every apply until cleared (clearing falls back to the declared domain);
  `port`/`component` are rejected. The route target is the declared
  `route = true` component or, implicitly, the single port-leasing
  process/docker component; the change is atomic — a refusal persists
  nothing. Observed: edits the observed route; when
  none exists yet, `port` is required (and `component` if several containers
  exist); a current-state re-import replaces such edits. Uniqueness is
  enforced across managed and observed routes.
- Imported current deployments return `observed_only=true`. Status, list,
  start/stop/restart, and logs work — lifecycle acts on the exact recorded
  container IDs and never recreates anything
  (DC2-2026-08-24-OBSERVED-LIFECYCLE); a missing container is reported, not
  replaced. apply/rollback/remove return `observed_only`; reviewed
  repository configuration is the explicit transition to configuration
  authority.

## health.containers (Phase 3, implemented)

Args: none. Result: `{containers: [{id, name, image, state, status, created,
repository_id, deployment_id, component, run_id, caller_uid, client,
ttl_seconds, data, classification}], counts}` where classification is one
of `managed-test`, `managed-preview`, `managed-permanent`,
`observed-current`, `orphaned-managed`, `unmanaged`. Classification uses
daemon-owned labels or an exact current observed-import identity
and recorded bindings only — never names, ports, images, or paths.

## health.summary | repositories | repository | history (Phase 4, implemented)

- `health.summary` → `{host: {cpu_percent, memory_total/used/available,
  swap_total/used, load_1/5/15, fs_size/free/used, ncpu, reconciliation:
  {managed_cpu_percent, daemon_cpu_percent, other_cpu_percent,
  managed_memory, daemon_memory, other_memory}}, storage: {fs_used,
  managed_repositories, devcoordinator_state, docker_shared, docker_images,
  docker_build_cache, docker_shared_volumes, other}, unhealthy_deployments,
  active_tests, container_counts, alerts, sampling}`.
- `health.repositories` → one row per repository with live `cpu_percent`,
  `memory_bytes`, `storage` buckets (checkout, test_scratch,
  deployment_artifacts, container_layers, volumes, postgres_data, total),
  `health`, deployments, and 12-point `trend_cpu`/`trend_memory`; plus the
  `devcoordinator` and `shared_unattributed` rows that complete the
  reconciliation `managed + DevCoordinator + shared/unattributed = host`.
- `health.repository {path}` → every measured subject of one repository
  (component, container, test) with live cgroup metrics and storage; dedicated
  PostgreSQL components carry `pg_connections`, `pg_wal_bytes`,
  `pg_temp_bytes`, `pg_database_bytes` (numbers only, never content).
- `health.history {subject_kind, subject_id, metric, minutes<=43200,
  points?}` → one-minute `{minute, min, avg, max, samples}` points, at most
  1440 per call (`truncated` flag), from the 30-day bounded store. With
  `points` (2..1440) the daemon downsamples server-side into that many
  buckets preserving the min/max envelope and sample-weighted averages —
  the Console's 24h/7d/30d charts use this. Host storage
  (`host/host/storage_bytes`) is persisted every storage tick.
- `health.summary.unhealthy_deployments[*]` carries `reasons:
  [{component, state, detail}]` naming exactly which component is unhealthy
  and why (recorded error or failing container healthcheck).

Alerts (in `health.summary.alerts` and as `alert.opened`/`alert.recovered`
events): host CPU > 90% for 5 min, host memory available < 10% for 5 min,
root filesystem < 10% free, component not running for 2 min while desired
running, ≥ 3 restarts in 10 min (crash loop), test scratch > 10 GiB.
Deduplicated while active; one recovery each.

## usage.repositories | repository (implemented)

- `usage.repositories {range?}` returns the registered repositories visible to
  the caller with compact combined Codex totals. `range` is exactly `24h`,
  `7d`, or `30d` and defaults to `24h`. The collection opens each configured
  source once for all requested repositories, reads only row-level token/count/
  execution facts through indexed identities, and has an 800 ms server-side
  query budget so repository count cannot multiply an individual timeout.
- `usage.repository {repository_id, range?}` returns one content-free combined
  report: coverage/source counts, provider-native top-level token categories,
  fixed UTC phase buckets, activity totals, separate timing unions, tool
  outcomes/families, and measurement semantics.
- Administrators may read every repository. Public operators may read only a
  repository where they hold operator-or-higher deployment access; viewers are
  denied. Results never include collector identity, paths, per-user values,
  raw Codex entity IDs, prompts, model output, source, commands, or payloads.
- `total_tokens` alone is the stacked/activity basis. Cached input and
  reasoning remain labelled subsets; missing collectors and unsupported source
  versions are partial/unavailable coverage, never synthetic zeroes.

## progress.repositories | repository (implemented)

- `progress.repositories {}` returns compact operator-visible repository rows:
  `{repository_id, display_name, open_tasks, tasks_done, planned_lines_done,
  planned_lines_total, next_release}`. Public scope is injected by the access
  guard; callers cannot pass `_repository_ids` themselves.
- `progress.repository {repository_id, period?}` accepts `period` exactly
  `hour`, `day`, or `week` (default `day`). Hour uses 24 one-hour buckets, day
  uses seven one-day buckets, and week uses eight Monday-aligned seven-day
  buckets; each response also compares the immediately preceding matching
  period.
- The result carries `series` buckets with task completions/arrivals/reopens,
  current planned lines completed, planned lines added, and net scope movement,
  terminal test counts and
  pass rate, provider `total_tokens`, and per-bucket token coverage. It also
  carries current/previous totals, current scope, source-specific coverage,
  a deterministic release forecast, open release work in depth-first Plan
  order, and explicit counting semantics. Each open-work row exposes only the
  owner-facing title and recorded status, estimate, elaboration request,
  unblock condition, and latest reopening note. The compact Console summary
  deliberately renders only title, status, estimate, elaboration, and a simple
  reopened state; full raw planning/event prose remains outside that surface.
- “Planned lines completed” means the current estimates attached to tasks whose
  permanent status event reached `done`; it is not measured Git churn.
  “Planned lines added” means initial task estimates plus later estimate
  increases; estimate reductions and dropped work remain part of net scope
  movement but are not presented as incoming work. Test history is bounded
  repository-local terminal metadata. Missing histories,
  unestimated tasks, no release, zero pace, and missing target dates remain
  explicit. The response does not infer priority, dependency, impact days, or
  an ordering scenario from those facts.
- Administrators may read every repository. Public operators may read only a
  repository where they hold operator-or-higher deployment access; viewers are
  denied because the result includes combined private token analytics.

## Public identities and roles (Phase 5, implemented)

A request carries `client.identity` only when the configured edge uid sends
it; any other peer is refused (`permission_denied`). Local callers stay
unrestricted. For identities: administrators may do everything;
`operator` may start/stop/restart granted deployments; `viewer` may read
status/logs/health of granted deployments; `access` only uses the deployed
application. `deployment.list` and `health.repositories` are filtered to
granted deployments; `health.summary`, `health.containers`, `test.*`,
`repository.*`, apply/rollback/remove, and user administration require an
administrator. Public callers address deployments by `deployment_id`.

- `user.whoami` → `{local, identity, user_id, administrator, grants}`.
- `user.list` → `{users: [{email, administrator, grants, …}], invitations,
  roles, owners}`.
- `user.invite {email, administrator?, grants?: [{deployment_id, role}]}`
  → `{invitation_id, email, expires_at}` (14 days; one exact identity).
- `user.accept_invitation {email, subject?, display_name?}` — edge only,
  for the signed-in identity itself → `{accepted, administrator, …}`.
- `user.remove {email}` → removes the user (and grants) or the invitation.
- `grant.set {email, deployment_id, role}` / `grant.remove {email,
  deployment_id}`.

Every user/grant change republishes the route document (owners + grants),
so edge enforcement changes on the next request.

## telegram.* and bug.* (Phase 6, implemented)

One server-owned bot (token in a private instance file, never in the
database or results). A Telegram user sends `/start` and receives a link
code; `telegram.link {code, email}` binds the chat to an identity (self or
administrator). `telegram.subscribe {chat_id, scope}` with scope `server`
(administrators), `deployment:<id>` (viewer or better), or
`repository:<id>` (a viewable deployment in it); `telegram.unsubscribe`;
`telegram.list` (own chats for public users; all for administrators).
Events: deployment apply/failure/rollback/start/stop/restart/remove,
component failure, preview expiry, test failure/timeout/supersession/
interruption (successful tests are silent), alert open/recover, new
unmanaged/orphaned container, daemon start, bug open/close, user and grant
changes. Delivery uses a bounded durable outbox (≤ 1000 rows, ≤ 10
attempts with backoff, ≤ 24 h) — reliability, never a work queue.

`bug.report {component, summary, expected, actual, steps, correlations?}`,
`bug.list`, `bug.close {bug_id}` — the daemon path only adds event emission;
the CLI and MCP write the same independent store directly so intake works
while the daemon, database, or edge is unavailable, then notify the daemon
best-effort (`notified: false` when it was down). Records are bounded atomic
text without secrets, raw logs, or private host paths; a recurrence
increments `occurrences` instead of duplicating; closing removes the record.

## plan.* / task.* / release.* / decision.* (Schemas 8, 11, and 12, implemented)

The glossary command family is documented separately in `docs/glossary.md`.
Its strict typed schema is `rust/api/src/glossary.rs`; CLI and MCP share
`glossary.list/resolve/get/save/configure/inherit/history/check/impact`.
Glossary records are not application messages, completion tasks or decisions.

DC2-owned planning, completion ledger, and decision history
(DC2-2026-08-24-PLANNING-LEDGER). Append-only: every task/release mutation
appends `plan_events` rows in the same transaction; nothing is ever deleted.
A daemon or database error from these commands blocks the affected
completion claim — there is no file fallback. Repository reference on
repo-scoped commands: `path` (agents; implicit registration like
`test.start`) or `repository_id` (Console). Plain-language fields (`title`,
`outcome`, decision `body`, release `name`) are structurally bounded and
written for a non-technical reader; `technical_note` is the separate
agent-facing field and never substitutes for them (the daemon cannot detect
jargon — the register rule lives in the agent instructions).

- `plan.overview {}` (no repo) → `{repositories: [{repository_id,
  display_name, open_tasks, loc_done, loc_total, current_release: {name,
  kind, status} | null, preview_requested, elaboration_request_count}]}` — the plan picker; for public
  identities filtered to repositories with a viewable deployment.
- `plan.overview {path | repository_id}` → `{repository_id, display_name,
  archived, merged_into_repository_id,
  releases: [{release_id, name, kind (preview|release), status
  (planned|requested|delivered|dropped), seq, note, requested_at,
  delivered_at, url, port, tasks_total, tasks_done, loc_total, loc_done}],
  tasks: [{task_id, parent_task_id, release_id, seq, position, title,
  impact (bounded excerpt), status, kind, estimated_loc,
  elaboration_needed}], tasks_truncated, elaboration_requests: [{task_id,
  title, outcome (bounded excerpt), status, kind, requested_at}],
  preview_requested: [{release_id, name, requested_at, note}], decisions:
  {unsummarized_count, summary_due}}`. One bounded call for the Console
  Gantt and agents; dropped tasks/releases are excluded (full row via
  `task.history`); aggregates count leaf tasks (a parent is a summary row);
  when the cap (500) cuts, every unfinished task is kept and
  `tasks_truncated` is true. `elaboration_requests` is independent of that
  cap, so a completed task's request cannot disappear with older chart rows.
- `task.create {path|repository_id, title, kind
  (goal|stub|improvement|user_feedback), outcome?, parent_task_id?,
  release_id?, estimated_loc?, impact?, unblock_condition?, verification?,
  technical_note?}` → `{task_id, repository_id, seq, position, status:
  "planned", release_id, elaboration_needed: false, preview_requested,
  elaboration_requests}`. `outcome` defaults to the
  title. The target release must still be open. Appends `created`.
- `task.update {task_id, title?, outcome?, impact?, unblock_condition?,
  verification?, technical_note?, estimated_loc?, status?, release_id?
  (null = backlog), parent_task_id? (null = root), position? (0-based order
  among siblings), elaboration_needed?, note?}` → compact task projection +
  `preview_requested` + repository `elaboration_requests`.
  Folds edits (`edited` event naming the fields), status changes (`status`;
  reopen is `done→in_progress`), estimate changes (`estimate`), release
  moves (`release_move`), reparenting (`reparent`; cycles rejected), and
  reordering (`reorder`; the sibling group is renumbered transactionally).
  `elaboration_needed: true` appends `elaboration_requested`.
  `elaboration_needed: false` is accepted only when the same update changes
  `title` or `outcome`, and appends `elaboration_completed` atomically with
  the `edited` event.
  `note` is stored on each appended event. Errors: `task_not_found`,
  `release_not_found`, `args_invalid` ("nothing to change" when a no-op).
- `task.history {task_id}` → `{task: <full row>, events: [{event, from, to,
  actor, at, note}], events_truncated, elaboration_requests}` — the bounded permanent history
  (last 200), loaded on concrete need.
- `release.create {path|repository_id, name, kind (preview|release), note?,
  seq?}` → `{release_id, repository_id, seq, name, kind, status:
  "planned"}`; an explicit `seq` must be free.
- `release.update {release_id, name?, seq?, note?, status?}` (administrator/
  CLI recovery) → compact release row. `status` may only move
  planned↔dropped; requesting and delivering are their own commands.
- `release.request {path|repository_id, name?, note?}` — the owner's ASAP
  button (Console-first; deliberately not an MCP tool): creates a
  `kind=preview, status=requested` release (default name "Preview
  (requested YYYY-MM-DD)"); a second pending request is refused. Agents see
  it in `plan.overview.preview_requested` and as `preview_requested` on
  every task mutation result.
- `release.deliver {release_id, deployment_id, note?}` → `{release_id,
  status: "delivered", delivered_at, url, port, commit_hash, dirty,
  generation_number}`. The deployment must belong to the same repository
  and have a current generation. Snapshots permanent reachability evidence
  (routed-domain URL when one exists, leased host port either way) plus
  commit/dirty/fingerprint — generations are pruned, the snapshot is not.
  A delivered release is immutable evidence; the next preview is a new
  release. Emits `release.delivered` (Telegram: owner gets the URL/port).
- `decision.record {path|repository_id, aspect (ui|architecture|algorithms|
  business_logic|data|testing|deployment|security|performance|process|
  other), title, body, technical_note?, ref?, supersedes?}` →
  `{decision_id, seq, ref, unsummarized_count, summary_due}`. `title`+`body`
  are the management-facing account; `ref` is a stable citation key (unique
  per repo); `supersedes` (id or ref) sets the old row's forward pointer —
  the record itself is never edited or deleted.
- `decision.tail {path|repository_id, aspect?, n? (1..50, default 10),
  before_seq?}` → `{repository_id, display_name, summary: {body,
  covers_through_seq, created_at} | null, decisions: [...], has_more,
  unsummarized_count, summary_due}`. The normal context load: rolling
  summary + last N; `before_seq` pages older windows (decisions with a
  lower sequence).
- `decision.search {path|repository_id, query, aspect?, n?}` →
  `{repository_id, query, decisions: [...], has_more}` — FTS5 full-text
  search over every decision (title, body, technical note, ref), bm25
  ranked; user text is quoted so FTS operators are literal.
- `decision.summarize {path|repository_id, body, covers_through_seq}` →
  `{repository_id, covers_through_seq, unsummarized_count, summary_due}`.
  Stores the agent-written rolling summary; all summaries are kept. When
  `unsummarized_count` reaches 25, every decision read reports
  `summary_due: true` and the working agent writes the next summary — the
  daemon never generates text. This authorized append-only maintenance acts
  directly and does not require a separate user confirmation.

Every repository-scoped task, release, and decision result also includes
`elaboration_requests`. Agents treat a non-empty list as owner input: load
each task, rewrite its title and/or outcome in everyday language, and clear
the flag in the same update. No automatic text generator or stand-in success
is implied.

Access: reads (`plan.overview`, `task.history`, `decision.tail`,
`decision.search`) require viewer on a deployment of the repository (public
callers reference by `repository_id`; a `path` would implicitly register
and is refused); all mutations require administrator; local socket callers
are unrestricted. New error codes: `task_not_found`, `release_not_found`,
`decision_not_found`.

MCP tools: `plan_overview`, `task_create`, `task_update`, `task_history`,
`release_create`, `release_deliver`, `decision_record`, `decision_tail`,
`decision_search`, `decision_summarize`. `release.request` and
`release.update` are owner controls (Console + CLI only). CLI:
`devcoordinator2 plan overview [--all]`, `task create|update|history`,
`release create|update|request|deliver`, `decision
record|tail|search|summarize`.

All follow the same envelope, error model, and file-reference conventions.
