# Migration and Cutover Runbook (Phase 8)

Every step here that changes the live public edge, fences the legacy
system, removes direct Docker access, or imports live state is executed by
the owner, deliberately, with a rollback path. Concrete names (domain,
accounts, legacy unit names, paths) are instance data kept in the untracked
`instance/` directory; this runbook uses placeholders.

## Rust cutover preparation

- `devcoordinator2-tooling install configure` creates or preserves the client
  group, edge account, instance configuration, private source policies,
  direct skill/policy links, and the edge's read-only access to the canonical
  checkout. `--canary` keeps the edge HTTP-only on the selected private port.
- `devcoordinator2-tooling install build` accepts only a clean canonical
  `main` equal to the already-fetched `origin/main`. It builds all three Rust
  executables as the checkout owner, probes their embedded commit, hashes
  them, and writes a root-owned mode-0600 candidate manifest. There is no
  copied release tree.
- `devcoordinator2-tooling install verify` rechecks the candidate manifest,
  embedded commits, hashes, and executable identities. `install plan` shows
  the exact unit and direct-link targets without changing the live service.
- `devcoordinator2-tooling legacy export` produced a reviewable read-only export of the
  legacy stores (no secrets) at `instance/legacy-export.json`.
- `devcoordinator2-tooling legacy import --dry-run` prints what would be imported and a
  deployment declaration plan; without `--dry-run` it imports
  administrators, Telegram chat links/subscriptions, and open bugs.

## First installation beside a legacy coordinator

After the verified build and configuration, use `install activate --bootstrap
--canary --yes --transaction-dir <new-private-directory>` for an instance
without installed v2 units, links, manifest, or daemon socket. This explicit
mode checks the daemon endpoint lease and preserves/backs up a prepared import
database; an absent database is initialized by the daemon. It never fences or
stops the separately named legacy coordinator. Failure removes only the new
installation assets and retains the database and recovery evidence.

The canary must be verified before public cutover. Existing `edge.env` values
are preserved by configure: deliberately replace the canary HTTP-only/port
settings with production settings before ordinary activation. Preserve the
existing certificate renewal webroot and credential loading paths as well.

`legacy import --native-routes <private-schema-1-document>` can preserve exact
exported route labels/ports for already-running listeners and stopped routes.
Every route requires its registered repository root, unchanged public/authenticated
boundary, and reviewed listener evidence. These are observed routes, not a
claim of container ownership or lifecycle management. Import is idempotent.
Existing admitted email identities must be migrated through the local user and
grant operations with their exact grants, not promoted to administrators or
left as expiring invitations. Approved historical requests are not pending work.

## Pre-cutover proofs (handover §20)

1. **Public authentication/grant boundary** — register the console
   redirect URI with the identity provider for the canary origin, fill
   `/etc/devcoordinator2/edge/oidc.client_id|client_secret`, restart the
   edge, sign in as the bootstrap administrator, invite a second identity,
   confirm a granted route works and a revoked one is denied.
2. **One permanent heterogeneous deployment** (worker + PostgreSQL) — add
   `[deployment.*]` declarations (from the import plan) to one repository,
   `devcoordinator2 deployment apply <path> --name <name>@checkout`, verify
   via Console; repeat for a `@worktree` instance if wanted.
3. **Immediate test with Docker/PostgreSQL cleanup** — `[test.<name>.postgres]`
   in that repository; `devcoordinator2 test start`, verify the container is
   gone afterwards.
4. **Attribution and health reconciliation** — `devcoordinator2 health
   repositories` / Console Health: managed + DevCoordinator + other = host.
5. **Telegram and bugs** — put the bot token in the file named by
   `DEVCOORDINATOR2_TELEGRAM_TOKEN_FILE` (mode 0600), restart the daemon,
   `/start` the bot, `devcoordinator2 telegram link --code … --email …`,
   subscribe, trigger a deployment event; `devcoordinator2 bug report …`.

## Cutover

6. **Backup** legacy state (authority DB, edge publication, access control,
   Telegram state) to a dated directory outside both repositories.
7. **Fence legacy mutations**: stop the legacy API/broker/test units
   (names in `instance/legacy-notes.md`) but leave its edge running.
8. **Import reviewed state**: `devcoordinator2-tooling legacy import --export
   instance/legacy-export.json --state-dir /var/lib/devcoordinator2
   --bugs-dir /var/lib/devcoordinator2-bugs`; apply the reviewed deployment
   declarations so every public route of step 2 exists with its domain;
   re-decide the pending access request (invite or ignore).
9. **Activate once**: configure without `--canary` after installing the TLS
   credentials, build and verify the frozen candidate, then run
   `devcoordinator2-tooling install activate --yes`. The command closes test
   admission while keeping the normal API, status, logs, and ledger reachable
   until accepted tests finish. It then fences the old socket, returns a
   reconnectable `daemon_unavailable` response to idle `event.wait` observers,
   waits for finite accepted requests to finish, rejects an
   applying deployment, creates the private SQLite backup and installation
   snapshot, switches the units and direct binary links, and verifies the v2
   daemon and Node edge. If interrupted, run
   `devcoordinator2-tooling install recover --yes`; it restores the captured
   installation and restores the database when integrity fails or the failed
   candidate changed its schema beyond the captured prior installation.
   Same-schema startup failures preserve intact current data.
   Subsequent source updates are developed and validated in worktrees, merged
   to `origin/main`, then fetched and fast-forwarded into the clean
   `/home/DevCoordinator2` checkout. A later `install build`, `verify`, and
   reviewed `activate --yes` cycle repeats the same provenance and drain gates.
   A running systemd process alone is not API readiness: verify the normal
   socket and a successful request after activation. Observers retain their
   last cursor and reconnect after the short activation window; they must not
   prevent installation indefinitely. Failed draining restores normal access
   and reopens admission without discarding accepted work
   (DC2-2026-09-07-REACHABLE-UPGRADE-DRAIN).
   The daemon and installer share the current database schema version, so an
   update remains installable after an earlier migration. The reviewed upgrade
   window includes every owned schema from 15 through the current version, so a
   version bump cannot remove the previously installed version. Installer tests
   activate each supported schema and reject older, malformed and future values.
   Activation also protects existing private authority files and recovery
   directories before restoring state-root traversal, then updates only the
   state-root tmpfiles rule. The rule is included in the installation snapshot.
   Database and backup creation retain private permissions from the first write
   (DC2-2026-09-12-OWNER-APPROVED-STATE-REPAIR).
10. **Docker authoritative mode** (owner decision DC2-…-DOCKER-MODE): remove
    agent accounts from the `docker` group, restart their sessions, verify
    `devcoordinator2 health containers` attributions; the observational
    label disappears from the security assumptions.
11. **Rollback**: prepare a revert in a worktree, merge it to `origin/main`,
    fast-forward the live checkout, and repeat the verified restart. Never
    rewrite or detach the live `main` checkout.

## Uninstalling the canary

`systemctl disable --now devcoordinator2 devcoordinator2-edge`, remove
`/etc/systemd/system/devcoordinator2*.service`, `/etc/tmpfiles.d/devcoordinator2.conf`,
`/usr/local/bin/devcoordinator2`, `/usr/local/bin/devcoordinator2-tooling`, any
inactive historical `/opt/devcoordinator2` source copies, and — only if the
state is not wanted — `/var/lib/devcoordinator2*`, `/etc/devcoordinator2`,
the `devcoordinator2-edge` user and `devcoordinator2-clients` group.
