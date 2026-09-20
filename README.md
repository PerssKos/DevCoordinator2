# DevCoordinator2

DevCoordinator2 is the server-wide authority for governed development tests,
deployments, health, planning work, and agent-facing operational coordination.
This repository is also the canonical source for six reusable agent skills and
one universal agent policy.

Clients can block on several typed owned-state filters with one `event wait`
request. A shared scheduler returns bounded authorized events, grouped elapsed
heartbeat deadlines, and a durable monotonic cursor; it does not prescribe any
client action or model agent/conversation wake-up behavior.

## Canonical agent assets

- `skills/dev-coordinator`: Coordinator CLI, MCP, planning, and runtime usage.
- `skills/formal-web-ui-verification`: deterministic rendered Web verification.
- `skills/full-repo-audit`: exhaustive implementation and contract audit.
- `skills/full-repo-test-coverage-audit`: structural and empirical test audit.
- `skills/ui-implementation-audit`: explicit implemented-UI audit.
- `skills/user-journey-docs-audit`: journey-documentation readiness audit.
- `reference/universal/AGENTS.md`: runtime-neutral universal agent policy.

The five audit/verification skills use the portable Rust tooling package. The
Coordinator skill is checked against this repository's Rust CLI and MCP
contracts.

## One live source checkout

`/home/DevCoordinator2` is the live source for the daemon, edge, command line,
skills, and policy. It remains a clean `main` fast-forwarded to `origin/main`.
All development happens in linked worktrees.

After a change is merged and validated:

1. fetch `origin` in the live checkout;
2. fast-forward `main` only;
3. verify the checkout is clean and equals `origin/main`;
4. drain active tests and restart services when runtime code or schema changed;
5. verify CLI, daemon, edge, database, skills, and policy.

Rollback is a new revert commit merged to `main`, followed by the same
fast-forward and restart sequence. Do not rewrite the live branch.

## Installation

The repository-owned installer configures systemd, the CLI shim, private
instance files, and direct agent links. It does not copy source into an
immutable release directory.

Use the reviewed managers for explicit runtime roots and policy targets:

```bash
devcoordinator2-tooling skills links plan \
  --repo-root /home/DevCoordinator2 \
  --target-root /absolute/runtime/skills

devcoordinator2-tooling skills policy plan \
  --repo-root /home/DevCoordinator2 \
  --transaction-dir /absolute/private/transaction \
  --codex-target /absolute/runtime/AGENTS.md
```

Apply only the reviewed plan using its required private transaction and digest.
Installed entries are direct absolute links to this checkout. Unrelated skills
and runtime files are preserved.

## Recovering saved planning history

`devcoordinator2 plan recovery` prepares a repository-scoped import from an
explicit activation snapshot. Supply `--repository-id`, `--transaction-dir`,
and the inspected `--backup-sha256`. The response contains counts, original-to-new
display-number mappings, and a `live_sha256`; it contains no saved record text.
`existing_counts` identifies matching records that will remain untouched;
`counts` excludes those duplicates. Events are matched by content and occurrence
count, not their database-local IDs. Different saved/live versions return
`status: conflicted`, a total `conflict_count`, and at most 64 `conflicts` naming
record IDs, changed fields and hashes without their values. Conflicts still
block application; a preview is not permission to overwrite either version.
Repeat the same command with `--apply --expected-live-sha256 <live_sha256>` only
for the reviewed result. Changed inputs and identity conflicts reject the entire
operation. Identical overlaps are retained separately as provenance without
turning existing releases into historical releases. A repeat of a completed import returns its permanent receipt without
replaying it.

For reviewed task conflicts limited to status, update timestamp and list position,
`--preserve-live-tasks <json-file>` accepts at most 64 objects with `task_id`,
`saved_sha256`, and `live_sha256` from the current conflict plan. Supply that
plan's `--expected-live-sha256` when preparing and applying this choice. Every
live task field stays untouched; both complete versions are retained as recovery
provenance. Other conflicts remain blocking. Counts and sequence mappings include
only missing records. `conflict_count` includes resolved conflicts, whose exact
choices are returned in `preserved_live_tasks`. The flag never selects a saved
status or discards the saved history.

Recovery preserves existing records, original saved identities, dates and
relationships. Historical releases remain visible but cannot select the current
release or replay an old preview request. Saved decision summaries are retained
as recovery provenance rather than replacing current summaries. After import,
review tasks through ordinary task/history/search operations and prepare a new
combined decision summary. A legacy snapshot without a recorded activation hash
remains explicitly labelled as such. This operation never performs an installation
rollback or restores unrelated runtime, account, route or credential data.

## Recovering a saved preview

`devcoordinator2-tooling deployment inspect-backup` reports the saved generation,
component bindings and ports for one explicit repository/deployment pair. It uses
the same immutable, hash-verified activation backup reader as planning recovery.

`devcoordinator2 deployment recovery --repository-id <id> --deployment-id <id>
--transaction-dir <saved-activation> --backup-sha256 <hash>` prepares recovery of
a private worktree preview containing processes and one owned PostgreSQL database.
It checks the exact running database, volume, private credential binding, original
ports and current ownership. Newer deployment generations and competing ports or
routes block recovery. Apply only the reviewed plan with `--apply
--expected-live-sha256 <hash>`.

Application data is streamed to a private PostgreSQL archive, validated and hashed
before ownership changes. Recovery keeps complete prior and saved metadata in an
immutable receipt, preserves the existing container and access grants, restores
the saved component ownership, and converts its routed port to a stable lease.
It withdraws the stale route until normal deployment apply proves the current
application and listener. It does not start application code, overwrite the
application database, clear another route owner, or constitute delivery evidence.
The retained archive is `recovery/<recovery_id>/postgres.dump` in private service
state. An interrupted or failed archive stays `postgres.pending` and is not
represented as verified. Repeating a successful recovery returns its receipt.

## Development and validation

If the system temporary filesystem is exhausted, use a short, private,
per-command `TMPDIR`. First verify free file entries, new-file user/group
ownership, and local socket creation. Inherited setgid directories and long
socket paths can invalidate fixtures. Do not change global temporary settings.

Product checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets \
  --features devcoordinator2-tooling/selftest-fixtures -- -D warnings
cargo test --locked --workspace \
  --features devcoordinator2-tooling/selftest-fixtures -- --test-threads=1
cargo build --locked --release \
  --package devcoordinator2-control \
  --package devcoordinator2-tooling \
  --package devcoordinator2-executor \
  --features devcoordinator2-tooling/selftest-fixtures
node --test edge/test/edge.test.mjs
node console/verify.mjs
```

Complete agent-skill and policy gate:

```bash
npm ci --ignore-scripts --prefix ci/playwright
target/release/devcoordinator2-tooling skills validate run \
  --root "$PWD" \
  --temp-root /absolute/external/temp/devcoordinator2-skill-validation
```

The skill gate verifies the exact six-skill inventory, shared policy,
runtime-neutral contracts, issue ledgers, ownership boundary, Rust harness
ownership, link-manager rollback, public artifacts, real browser fixtures, and
all six skill packages. It rejects executable Python and permits only the seven
explicit inert cross-language audit fixtures.

Linux root acceptance is a separate, feature-gated test binary and never uses
the installed daemon:

```bash
cargo build --locked --release \
  --package devcoordinator2-control \
  --features root-acceptance \
  --bin devcoordinator2 \
  --bin devcoordinator2-root-acceptance \
  --package devcoordinator2-executor \
  --bin devcoordinator2-executor \
  --package devcoordinator2-executor-core \
  --bin devcoordinator2-executor-test-fixture

sudo -n env DEVCOORDINATOR2_ROOT_ACCEPTANCE=1 \
  target/release/devcoordinator2-root-acceptance run \
  --daemon target/release/devcoordinator2 \
  --executor target/release/devcoordinator2-executor \
  --fixture target/release/devcoordinator2-executor-test-fixture \
  --work-root /absolute/private/empty-root-acceptance-directory \
  --report /absolute/private/root-acceptance-report.json
```

Build this feature-gated daemon after ordinary workspace tests, which can replace
the same output path with a daemon that ignores fixture executor overrides. Keep
these binaries unchanged while root acceptance runs.

The harness owns a unique socket, database, port range, systemd unit prefix,
Docker label namespace, and marker-bound filesystem root. It runs all 30
preserved real-system scenarios with all-settled behavior and never targets the
live service, socket, containers, or data.

If Docker's default address pools are exhausted, `--compose-subnet` accepts an
explicit private IPv4 `/24` network for the isolated Compose fixtures. Verify
that the selected subnet overlaps neither host routes nor existing Docker
networks before running. This does not change Docker's global pools or reuse
shared networks; each sequential scenario retains its owned-network cleanup.

## Imported source provenance

The reusable agent assets were imported as a current-tree snapshot from the
retired Holy Skills repository. See `docs/holy-skills-snapshot.md`. Its Git
history remains available in the archived source repository; it is not merged
into this repository's ancestry.
