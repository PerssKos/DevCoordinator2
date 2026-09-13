# Repository Configuration: `.devcoordinator.toml`

One small reviewed file at the repository root. Configuration is canonical
for commands and component meaning; the coordinator database is canonical
for live assignments, identities, state, users, grants, and observations.

## Governed-test schema 2

```toml
schema = 2

[test]
default = "unit"            # optional; required if more than one test

[test.unit]
timeout_seconds = 600       # outer systemd containment watchdog
cwd = "."                   # optional default for checks, repo-relative
env = { CI = "1" }          # optional default, string→string; additive only

[[test.unit.check]]
name = "unit"
tier = "development"
command = ["cargo", "test", "--locked", "--workspace"]

[test.unit.postgres]        # optional: test-scoped ephemeral PostgreSQL
image = "postgres:16-alpine"   # official postgres:<tag> (default), or a compatible
                                # image pinned as name@sha256:<64 lowercase hex>
database = "test"              # [a-z_][a-z0-9_]{0,62}
user = "test"

[test.complete]
timeout_seconds = 21600

[[test.complete.check]]
name = "build"
tier = "development"
command = ["npm", "run", "build"]
produces = ["dist/app.js"]  # immutable regular-file receipts, repo-relative

[[test.complete.check]]
name = "source-preflight"
tier = "development"
role = "preflight"
command = ["./scripts/check-source-integrity"]
invalidates = ["server", "browser"]
timeout_seconds = 60        # failure ceiling for this leaf

[[test.complete.check]]
name = "server"
tier = "pre-merge"
command = ["./scripts/start-test-server"]
requires = ["build"]
completion = "event"        # emits its exact event, then may stay alive
on_failure = "stop"         # only for evidence-invalidating/unsafe failure

[[test.complete.check]]
name = "browser"
tier = "release"
command = ["node", "verify.mjs"]
requires = ["server"]
timeout_seconds = 900
retained_artifacts = [
  { name = "production", path = "artifacts/browser", max_bytes = 536870912 },
]
diagnostic_sources = [
  { format = "playwright-json", path = "playwright/report.json" },
]

[[test.complete.check]]
name = "locale-cases"
tier = "pre-merge"
discover = ["./scripts/list-locales"]
case_command = ["./scripts/check-locale"]
requires = ["build"]
```

Every dependency-ready check or expanded case enters the host-wide adaptive
admission queue immediately. Repository configuration has no `max_parallel`,
worker-budget, resource-lock, CPU, memory, or client-override field. If two
checks cannot safely overlap for correctness, declare their real completion or
success dependency. Every leaf receives an isolated
`DEVCOORDINATOR_CHECK_SCRATCH`, the shared
`DEVCOORDINATOR_SHARED_ARTIFACTS`, a private
`DEVCOORDINATOR_DIAGNOSTICS_DIR`, a dedicated inherited structured-diagnostic
descriptor, and its exact run/check/case identity.

`diagnostic_sources` optionally declares structured reports written below the
leaf-specific `DEVCOORDINATOR_DIAGNOSTICS_DIR`. A declaration contains exactly
`format` and `path`; formats are `junit`, `playwright-json`, and `rust-json`.
Paths are normalized relative paths below that diagnostics directory, never
repository or host paths. The Rust executor extracts only bounded typed failure
fields for ordinary completion; raw report messages, stacks, stdout, and stderr
remain cold evidence available through explicit bounded log reads.

`completion = "process"` (default) uses the exact exit status. A long-lived
setup uses `completion = "event"` and emits one identity-bound result with
`devcoordinator2 test event passed|failed|unsafe`; the inherited descriptor,
not elapsed time, binds the event to that check. A passed long-lived process
stays available to dependents and is terminated during final cleanup. If it
exits early, downstream evidence is unsafe. `produces` paths are content-hashed
regular files; symlinks, missing files, path escape, and mutable receipts fail.

`retained_artifacts` is available only to direct process-completed checks and
declares required repository-relative directories to snapshot after success.
Each table contains exactly `name`, `path`, and `max_bytes`. The source tree may
contain only regular files and directories, cannot be empty, aliased,
overlapping, `.git`, or `.devcoordinator`, and must remain unchanged while it is
copied. At most eight trees and 4,096 files per tree are retained; one tree is
limited to 1 GiB and all declared ceilings to 2 GiB. The private copy, per-file
hashes, and whole-tree digest expire with the governed run. This is release
evidence, not reusable setup output; `produces` remains the regular-file reuse
contract.

Every check declares its minimum `tier`: `development`, `pre-merge`, or
`release`. Tier selection is cumulative; only a fresh complete release run is
readiness evidence. `role = "preflight"` permits `invalidates` targets, which
compile to success dependencies and become `invalidated` when the preflight
fails. `timeout_seconds` on a check or expanded case is only a failure ceiling;
it never means success.

One-level case expansion uses either static `cases` plus `case_command`, or one
terminating `discover` command plus `case_command`. Discovery writes a single
JSON manifest to the inherited descriptor (maximum 2 MiB and 4,096 unique
bounded case IDs). Each case contributes only an argument array appended to the
reviewed `case_command`; it cannot replace the command, cwd, environment, or
expand recursively.

Each direct check, discovery step, and expanded case writes byte-complete
stdout and stderr directly to its own stable run folder. There is no stream
size cap and no interleaved aggregate copy. Repository configuration does not
control retention: administrators own the host-wide age and history-depth
boundaries, which default to 24 hours and three runs for each logical case.

An ephemeral PostgreSQL is one throwaway instance per run: a Docker
container carrying the exact run identity in daemon-owned labels, data on
tmpfs, published on loopback only, credentials generated per run. The test
process receives `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`,
and `DATABASE_URL` through a caller-owned 0600 environment file (never via
argv or the unit's public environment). The container is removed on
completion, timeout, cancellation, supersession, daemon recovery, or the
next start. The Coordinator never copies declared or injected environment
values into summaries, metadata, metrics, or agent results. Commands must not
print those values because their stdout and stderr are retained byte-completely
as private cold evidence.

Official PostgreSQL tags use the established preloaded-image path. A compatible
image outside that namespace must be immutable: the daemon pulls the exact
digest when absent, verifies the local repository digest, and then runs with
`--pull never`. Mutable derived-image tags are rejected. The image must honor
the standard `POSTGRES_USER`, `POSTGRES_PASSWORD`, and `POSTGRES_DB` entrypoint
contract and provide `pg_isready`; extensions remain repository-specific.

Validation rules:

- `schema` must be `2`. Schema 1, direct test commands, translation, and
  compatibility fallback are rejected.
- Every test declares one or more `[[test.<name>.check]]` tables. Every direct,
  discovery, and case command is a non-empty argv array. Shell strings are
  forbidden everywhere.
- `cwd` must resolve (realpath, after joining) inside the repository; `..`
  or symlink escape is rejected.
- Test and leaf `timeout_seconds` values are integers in [1, 21600]. The test
  value is outer containment; a leaf value is a failure ceiling.
- `env` values must not look like secrets (no key names matching
  token/secret/password/key patterns with literal values — reference
  secrets held outside the repository instead).
- Test names: `[a-z0-9][a-z0-9-]{0,31}`.
- Check names: `[a-z0-9][a-z0-9-]{0,63}`; names are unique, dependencies must
  exist, self-dependency and cycles are rejected, and `after`/`requires` may
  not repeat the same edge.
- Check `tier` is required and is `development|pre-merge|release`; a dependency
  cannot require a check excluded from its selected tier closure.
- Check `role` is `work|preflight` (default `work`). Only a preflight may
  declare non-empty `invalidates`; targets must exist, cannot invert tier
  closure, and become success-dependency edges before cycle validation.
- A check declares exactly one of `command`, `discover`+`case_command`, or
  static `cases`+`case_command`. Fan-out manifests and cases are finite,
  bounded, uniquely named, argument-only, and non-recursive.
- Check `completion` is `process|event`; `on_failure` is `continue|stop`.
- A check may declare at most eight unique `diagnostic_sources`. Every entry
  has exactly one supported format and one normalized leaf
  diagnostics-relative path; absolute paths, traversal, and backslashes are
  rejected.
- A direct process check may declare at most eight non-overlapping
  `retained_artifacts` tables. Names are unique check-style identifiers; paths
  are normalized repository-relative directories outside `.git` and
  `.devcoordinator`; `max_bytes` is a positive integer no greater than 1 GiB.
  Fan-out and event-completed checks cannot retain generic artifact trees.
- `DEVCOORDINATOR_*` environment names are reserved for exact runner identity,
  scratch, artifact, diagnostic, log, manifest, and event delivery.
- Unknown keys anywhere are rejected (`repository_config_invalid`), so
  typos never silently change meaning.

## Forbidden content (rejected by validation)

- Host filesystem paths outside the repository or approved deployment
  roots.
- Literal secrets of any kind.
- Docker socket operations, privileged flags, or raw Docker options.
- CPU/memory/PID admission values.
- Repository-selected retry, queue, evidence-retention, concurrency-budget,
  or resource-quota policies. Diagnostic selection/retry is a Coordinator
  command with fixed semantics, not repository authority.
- Public users and grants.

## Phase 3 schema: deployments (implemented)

```toml
[deployment.web]
source = ["checkout", "worktree"]   # "worktree" (default; live checkout), "checkout" (immutable
                                    # generations), or both — each enabled source is an independent
                                    # deployment instance (own identity, ports, data, generations)
domain = { checkout = "app", worktree = "app-dev" }   # per-source labels under the instance base
                                                      # domain; a plain string is allowed with one source
components = ["db", "api", "worker", "cache"]   # declared order; stop is reverse
build = ["npm", "run", "build"]                 # optional argv run as the caller before components start
ttl_seconds = 86400          # optional: temporary (preview) deployment, auto-stopped when expired
public = false               # true: the route needs no sign-in at the edge

[deployment.web.component.db]
type = "postgres"            # dedicated instance; persistent named volume, never deleted by stop/redeploy
image = "postgres:16-alpine"
database = "app"
user = "app"
# shared_from = "<deployment_id>/<component>"   # use another deployment's dedicated instance instead

[deployment.web.component.api]
type = "process"
command = ["npm", "run", "start"]   # argv only
cwd = "."
port = true                  # daemon leases a host port, injected as PORT
route = true                 # the domain routes to this component (optional when exactly one process/docker component leases a port)
health = { path = "/healthz", timeout_seconds = 60 }   # or { tcp = true, timeout_seconds = 30 }
env = { NODE_ENV = "production" }
depends_on = ["db"]          # narrow ordering within the declared order
independent_control = true   # default true: may be started/stopped/restarted alone
persistent_paths = ["var/data"]   # repo-relative data the product must never delete

[deployment.web.component.worker]
type = "process"
command = ["npm", "run", "worker"]
depends_on = ["db"]

[deployment.web.component.cache]
type = "docker"
image = "valkey/valkey:9.1.0-alpine"
command = []                 # optional container argv
env = {}
port = 6379                  # optional container port; published on a leased loopback port
volumes = ["data:/data"]     # named persistent volumes (daemon-owned names), never deleted by stop/redeploy

[deployment.web.component.stack]
type = "compose"
files = ["compose.yml", "compose.dev.yml"]  # or singular file = "compose.yml"
env_file = "deploy/dev.env"  # optional ignored repo-relative interpolation file;
                             # requires separate private instance authorization
services = ["bootstrap", "api", "worker"]  # optional subset; required with roles below
finite_services = ["bootstrap"]             # must exit 0 once per changed apply
independent_services = ["worker"]           # reviewed long-running service controls
build = true                 # build on apply; ordinary start never builds
port = true                  # leases PORT for Compose interpolation
route = true
timeout_seconds = 300        # 1..900; up to 21600 when finite_services is non-empty

[deployment.web.component.smtp]
type = "external"            # observed only, never owned or controlled
tcp = "127.0.0.1:25"
```

Explicit finite services may use a longer bounded execution deadline, including
when accompanied by an artifact server. The default remains 300 seconds; an
ordinary component without finite services remains limited to 900 seconds.

An explicitly all-finite Compose component may list every selected service in
`finite_services`. It completes with state `completed`, not `running`, only when
every selected service exits successfully. It cannot lease a service port,
publish a route, or declare a running-service health probe. Existing mixed
Compose deployments still require their non-finite services to remain running.
Unchanged apply and ordinary start preserve completed work instead of rerunning
it. A changed apply creates a new execution; failures retain diagnostics and do
not become completion evidence.

For a deployment composed entirely of finite workloads, a whole-deployment stop
can cancel an active apply. `stopping` means the request is accepted but cleanup
has not settled; `cancelled` is reported only after the owned candidate cleanup
succeeds. Cancellation cannot reverse an operation already sealed for commit.
No agent Docker access or runner privilege relaxation is involved. Explicit
deployment removal retains the normal exact-target/data-deletion controls.

Environment injected into every component of a deployment:
`DC2_DEPLOYMENT`, `DC2_COMPONENT`, `DC2_GENERATION`, `PORT` (own leased
port when `port` is set), `DC2_PORT_<NAME>` for every leased port in the
deployment, `DC2_POSTGRES_<NAME>_URL` for every dedicated or shared
PostgreSQL component, and `DATABASE_URL` when exactly one exists. Generated
PostgreSQL credentials live in private daemon state and reach components
only through 0600 environment files or container environment.

Instances are addressed as `<name>@<source>` (`web@checkout`, `web@worktree`);
a single-source deployment may be addressed as just `<name>`. A worktree
instance may use the checkout instance's database via `shared_from`.

Validation adds to the Phase 1 rules: component names `[a-z0-9][a-z0-9-]{0,31}`;
every listed component has a table and vice versa; `depends_on` references
earlier components only; at most one `route = true`. When `domain` is set and
no component declares `route = true`, a deployment with exactly one
port-leasing process/docker component routes to it implicitly (2026-08-24);
with several, `route = true` is required. `domain` labels
`[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?`; docker images
`name[:tag]` without privileged flags, host mounts, or socket access;
every `compose.files`/`compose.env_file` path stays inside the repository;
`finite_services` and `independent_services` are duplicate-free subsets of an
explicit `services` list and never overlap; at least one long-running service
remains. A changed apply removes/recreates the finite service containers,
requires successful completion, and retains bounded generation receipts.
Unchanged convergence and ordinary start/restart do not rerun finite services. An
independent service is addressed as `<component>/<service>` and exact-container
start/stop/restart never follows dependencies or touches unrelated services.
`env_file` remains inert unless private instance configuration separately
authorizes the exact deterministic repository ID and normalized relative path;
the file must remain ignored, regular, and non-symlink on every use. Its values
never enter repository configuration, Coordinator metadata, results, logs, or
argv. Because ignored files are deliberately absent from immutable checkout
generations, this exception is available only to worktree deployments.

Use `deployment preflight` to inspect all missing environment authorizations
before apply touches runtime resources. Use `config authorize` only after the
exact grant is approved; it can activate an existing policy without a daemon
restart. After source or migration changes, use apply rather than restart.
Explicit status distinguishes the running components from current-source
readiness through `pending_apply` and `missing_components`. Completion receipts
retain the actual finite-service generation and execution timestamps; their
later observation time is not a new execution.
`shared_from` is exclusive with `image`/`database`/`user`.

## Reserved (later phases)

User, grant, Telegram, and route-publication settings are never repository
configuration.
