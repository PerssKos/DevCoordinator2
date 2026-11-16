# PostgreSQL and protocol-v3 acceptance contract

This is the release verification contract, not a completion ledger. Coordinator
owns unfinished outcomes and the receipts establishing completion. A passing
source build or an imported database alone does not establish acceptance.

The frozen candidate must preserve the operation catalog's complete capability
set and authorization policies. Generated schemas and edge metadata must match
source. CLI discovery and typed JSON calls, MCP discovery/calls, Unix sockets,
sandbox transport and the Console must agree on protocol 3 and source identity.
Protocol 2 and the previous hand-written CLI grammar must be rejected.

| Boundary | Required evidence |
| --- | --- |
| Authority migration | Read-only SQLite source; every table's counts and canonical typed hashes agree; IDs, sequence high-water marks, JSON numbers, signed bytes, attachments, foreign keys and append-only history preserved; FTS rebuilt; representative plan/task/decision/review/delivery/hub/storage reads agree. |
| Native persistence | Transaction rollback, checked type conversion, read-only lane, bounded connection exhaustion, statement/lock timeout, PostgreSQL restart, idempotent schema application, dump/restore, and isolated snapshot inspection. |
| Responsiveness | Hold both maintenance connections and workers busy while actual daemon plan/task/decision/health/status reads and mutations continue. Cheap-read p95 below one second. Exercise retention and partition DDL under request traffic, including at least six million retained metric rows. |
| Maintenance | Bounded rows, bytes, wall time, queue and concurrency; priority; cursor persistence; cancellation; retry without skipping work; daemon/scheduler restart; no transaction held between batches; exact 7/30-day retention; large-directory and log-tree traversal cannot monopolize workers or accumulate unbounded state. |
| Functional journeys | Repository registration and archival, planning and decisions, releases and delivery qualification, deployment lifecycle/recovery, governed tests/logs/evidence/waits, health/incidents, usage, storage/cleanup, review scheduling, sketches/continuation/annotations, tickets, routing and agent hub all remain real. |
| Authorization | Existing trusted local and public edge identities, invitations, grants, viewer/operator/administrator boundaries, denied operations and private evidence rules are unchanged. |
| Browser and edge | Formal rendered journeys, complete required viewport/state cells, manual screenshots, applicable existing mockup audit, actual authentication where required, and live v3 source identity. Earlier source or fixture-only evidence cannot prove the live cutover. |
| Failure and recovery | Interrupted import is atomic; nonempty destination refused; failed cutover before new writes restores the old daemon and exact frozen state; accepted finite work survives drain; after admitting new writes all recovery uses PostgreSQL. |
| Source size and capability | Compare tracked first-party source lines and catalog capability coverage with the original commit. Remove the SQLite actor, runtime SQLite schema, duplicated operation maps, repeated hand CLI converters and ad-hoc maintenance loops. Verified PostgreSQL/v3 delivery takes priority over total line reduction: report any net increase explicitly and preserve all capabilities. Do not count moved fixtures as deleted functionality. |

One isolated full rehearsal precedes the live switch. Before the switch, stop
new mutations with the installation fence, finish accepted finite work, flush
maintenance, and create and verify the temporary frozen SQLite backup. Activate
PostgreSQL and v3 daemon/clients together. Run source and live acceptance against
the same identity while rollback remains safe, then verify a PostgreSQL recovery
backup. Only complete acceptance permits removal of the old authority SQLite
database and temporary migration backup. Keep PostgreSQL recovery backups and
private evidence outside source; never remove unrelated external usage databases,
test state, bug files, or older owner data.

Validation evidence records the actual command, candidate source, test counts,
bounded failure index, latency measurements and private artifact references.
An unrun, failed or blocked check stays explicit; it cannot be replaced with a
successful startup, submitted job, metadata release, or inferred outcome.
