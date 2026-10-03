# Tracked storage cleanup

Storage inventory separates data that is safe to remove from data that is in
use, protected, awaiting observation, or needs ownership review. Every artifact
has a concrete reason. An old timestamp or stopped deployment is never enough
to authorize deletion. Current declarations protect their resources even when
their services are stopped.

## Inspect and remove

Use `devcoordinator2 storage --help` for the current grammar. Start with
`storage scan --idempotency-key <unique-key>` and follow the returned `job_id`
through `storage job status` and `event wait`. Discovery runs in the background
under the shared admission broker. `storage inventory` supports project,
filesystem, kind, safety and name filters, with bounded offset pagination.
`storage show <artifact-id>` exposes safety, ownership, size, last verified use,
dependencies and the effective deletion deadline. `storage history` retains
per-item results after data removal.

Protect an artifact with `storage protect <artifact-id> --expected-revision
<revision>`; use `--remove` to remove that explicit pin. Removing a pin does not
override a current deployment, consumer, lease, required backup or missing
observation. Leases use `storage lease set --file <request.json>` and
`storage lease release <lease-id>`; clients renew them while tracked data is
needed. Lease duration is bounded to one day per renewal.

Prepare exact targets with `storage cleanup plan --artifact-id <id>` (repeat
the option for more targets). Include `--include-persistent-data` only when the
existing authorization covers permanent data. The returned plan lists every
consumer, mount and backing directory needed for removal and each blocking
reason. Review the target names and effects, then call `storage cleanup start
--plan-id <plan-id> --idempotency-key <unique-key>`. The same key returns the
original job on retries. Follow its events and retain its terminal receipt;
`storage job cancel <job-id>` prevents subsequent steps but does not undo
already completed removals.

Selecting a directory also lists known nested directories affected by that
removal. Their protection and ownership checks still apply. Aliases and nested
data contribute to the planned space only once; each tracked identity retains
its own result even when one containing-directory removal handles them together.

## Legacy Docker ownership

Observed deployments remain read-only through ordinary deployment controls.
For an independently verified retired group, call `storage legacy-register
--deployment-id <id> --expected-inventory-revision <revision> --reason <reason>`.
The reason records the existing disposal authorization. Registration refreshes
native safety evidence for all known members and grants cleanup ownership as
one transaction; it does not recreate or adopt the deployment as an application.
New or unrecorded consumers must be resolved by discovery before removal.

The cleanup graph removes stopped container consumers, retires matching bind
and automount entries, removes Docker volumes, then removes their backing
directories. Docker forbids removing a volume still referenced by a container
([Docker reference](https://docs.docker.com/reference/cli/docker/volume/rm/)).
An automount must also be stopped, or later access can activate its mount again
([systemd reference](https://www.freedesktop.org/software/systemd/man/latest/systemd.automount.html)).

The daemon retains private resource definitions and mount-configuration recovery
metadata; it does not make another large copy of data declared disposable.
The native maintenance helper accepts persisted job and artifact identities,
verifies its installed source/binary manifest and the running cleanup plan,
preserves unrelated configuration, and rejects a changed configuration. It
does not broaden the ordinary daemon's filesystem sandbox.

## Policies, roots and recovery

The local Docker builder uses the Engine's structured cache inventory and
exact-ID removal API, including on hosts whose Buildx version lacks JSON
formatting. Active and shared records remain blocked. Raw build descriptions
are private because they can contain command arguments. Remote builder
endpoints require a provider and are reported as unavailable. Root acceptance
uses a separate Docker daemon and a private network namespace for these checks.

Global defaults are three inactive days for build outputs, dependencies and
caches, and fourteen for other unused disposable resources. Use `storage policy
show` or `storage policy set --file <request.json>` for global settings; include
`repository_id` for a project override. Unknown past activity starts at the
first verified observation. Current use and leases extend the inactivity
baseline. Hourly discovery and relevant lifecycle events refresh the inventory;
eligibility deadlines trigger another observation before automatic deletion.
Unused volumes retain verified ownership from their managed labels or earlier
recorded consumers only while their creation identity and backing inode still
match. Removing the final container does not erase that evidence. Volumes with
unknown ownership remain blocked, including after the inactivity deadline.
Discovery and state changes carry a persistent sequence. A slow earlier scan
cannot restore removed inventory or overwrite newer use, protection or failed
safety checks, including across a daemon restart.

`storage roots set --file <request.json>` declares a directory containing
generated outputs, dependency caches, backups or unrecognized data. Roots bind
the actual directory identity; links and replacements require fresh review.
Unrecognized directories are visible but blocked until `storage register`
records verified ownership and disposal intent. Canonical sources, credentials,
chat history and the authoritative Coordinator database remain protected.
Generated directories containing recognized credential or chat metadata are
blocked even when the directory was explicitly declared as rebuildable. The
check examines names rather than secret contents; public examples such as
`.env.example` remain eligible. Verified backup copies retain their own
recovery-generation policy.

Worktrees need an explicit disposal declaration, no current owner/runtime use,
a clean Git state, no unique commits and a verified remote baseline. Backup
cleanup preserves at least two verified generations per recovery lineage plus
explicit pins. Generic backup roots contain `backup-manifest.json` with a
`lineage`, `created_at_ms`, and nonempty `files` entries naming relative `file`
paths and their `sha256` digests. Native installation snapshots use their
existing database verification. Missing or damaged evidence never counts toward
the required recovery floor.

Governed test evidence remains under its existing age/depth retention service.
Storage protection and active leases, required release/review evidence and open
feedback references prevent expiry. Manual evidence removal uses that same
retention engine and leaves a compact cleanup receipt.

Receipts distinguish removed, blocked and partly removed items. A persisted
intent allows the daemon to reconcile exact absence after a restart without
deleting a replacement. Space readings report what was actually observed on
the affected filesystem; concurrent writes and unmeasured resources remain
explicit. Inventory size alone is not proof of reclaimed space.
