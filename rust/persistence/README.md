# PostgreSQL authority persistence

The daemon uses an explicitly configured local Unix-socket PostgreSQL database.
TCP endpoints and a writable SQLite fallback are rejected. Connection strings
come from a private mode-0600 JSON file owned by the service identity:

```json
{"schema":1,"connection":"host=/absolute/socket/directory user=authority dbname=authority"}
```

`DatabaseConfig::from_env` reads only `DEVCOORDINATOR2_DATABASE_CONFIG`, the path
to that file. Its debug output omits the connection string. The control crate's
configuration adapter applies the same private-file boundary.

`Persistence` has independent read, write and maintenance pools, defaulting to
8, 4 and 2 connections. Pool acquisition, statements, locks and idle
transactions have positive timeouts. Session settings apply before implicit
statements as well as explicit transactions. Read connections enforce
PostgreSQL read-only behavior. Transaction guards roll back on callback errors
and unwinding. Control's `Database` boundary also isolates synchronous calls and
final connection cleanup from both Tokio runtime flavors. Sessions disable JIT
compilation because its setup cost can exceed the entire deadline for small
authority queries.

The `Connection`, `Statement`, `Row`, `params!` and `OptionalExtension` helpers
only bind native PostgreSQL values and map results. SQL must use native `$1`
parameters and PostgreSQL syntax; no SQL translation is performed. Integer
decoding checks the requested range. Legacy 0/1 flags retain their integer
domain. Serialized records use JSONB; signed Agent hub outbox payloads retain
their exact original TEXT bytes. JSON numbers keep arbitrary decimal precision.

Ordered native migrations live in `migrations/` and are SHA-256 bound in
`dc2_schema_migrations`. Schema 35 includes every ordinary legacy schema-34
table and its dynamically added columns, append-only history protections,
indexed text and JSONB search, daily metric partitions with a historical
fallback partition, and durable maintenance jobs. Private `rowid` identities
retain legacy ordering without becoming product identifiers.

The offline migration tool supports `--mode inspect`, `--mode import` and
`--mode verify` (alias `verify-import`), with `--source`, `--database-config` and
a new `--report` path. Archive modes `backup`, `verify-backup`,
`verify-recoverable-backup` and `restore-candidate` use `--destination` instead
of a SQLite source. `inspect-postgres` observes an existing schema without
migrating it; `migrate` applies native migrations explicitly. `verify-routes`
also requires `--state-dir` and `--edge-state`.
It reads a frozen schema-34 SQLite file with immutable/read-only flags, refuses
uncheckpointed WAL/journal data, hashes the complete source and schema metadata,
and streams ordinary tables in stable foreign-key order through binary COPY.
Virtual/shadow indexes are classified by SQLite's catalogue, not by guessed
name patterns. Native search documents are rebuilt separately.

Import requires an untouched bootstrap target, checked against its exact
default rows; it never overwrites an existing authority database. One
transaction compares every table's ordered typed checksum and row count,
including blobs and semantic JSON numbers, validates foreign keys, restores
identity sequence high-water marks, and stores an immutable receipt. A failed
copy rolls back. `verify` repeats preservation checks after restart. Neither
inspection nor import deletes the source; source retirement belongs to the
separate accepted cutover procedure. Offline import and verification have a
finite thirty-minute operation allowance on maintenance connections, separate
from request deadlines.

Native backups are private, atomically published `pg_dump` custom archives.
`backup::with_snapshot` restores an archive into one uniquely owned scratch
database on the configured cluster, verifies archive identity/hash before and
after, supplies a read-only callback, and removes only that database on success,
failure or unwinding. This needs database-creation authority on the dedicated
cluster. Legacy SQLite backups remain read-only migration inputs in the
control-layer adapter.

`verify-backup` compares every restored ordinary table and sequence against the
frozen authority. After live writes resume, `verify-recoverable-backup` checks
the hash-bound isolated restore, foreign keys, schema metadata, and generated
identity heads without claiming equality to later writes. `restore-candidate`
refuses an existing database containing any user relation, routine or type.

Tests use one RAII disposable cluster runtime with private Unix sockets and no
TCP listener. Server binaries must be installed under `/usr/lib/postgresql/`
or selected explicitly through `DEVCOORDINATOR2_TEST_POSTGRES_BIN`; no installed
Coordinator daemon or live database is consulted. Root execution additionally
requires explicit trusted non-root UID/GID values. Root acceptance uses
`FixtureCluster::start_in` inside its new marker-bound external work root.

```sh
cargo test -p devcoordinator2-persistence --no-fail-fast
cargo test -p devcoordinator2-control --lib database::tests::
cargo test -p devcoordinator2-persistence --test metric_retention -- --ignored
cargo test -p devcoordinator2-persistence --test sqlite_cutover -- --ignored
```

The final two commands execute real six-million-row retained-history and
offline SQLite COPY/restart/typed-verification fixtures.
Retention returns one bounded transaction receipt: at most 256 selected rows,
a 256 KiB selection budget (one oversized row may progress alone), and at most
one whole-partition drop. Provisioning creates one detached partition and
attaches it per transaction; DDL has 25 ms lock and 100 ms statement deadlines.
Exact deleted-row counts and partition removals are
reported separately; expiry never counts or scans a whole retained table.
