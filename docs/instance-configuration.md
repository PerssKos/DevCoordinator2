# Instance Configuration

All installation-specific values live outside the committed repository:

- Installed daemon: `/etc/devcoordinator2/instance.env`
  (root-owned, mode 0640, group = the daemon client group).
- Development: an untracked `.env` in the checkout root (gitignored), or
  `DEVCOORDINATOR2_*` environment variables directly.
- Human notes (accounts, legacy paths): the untracked `instance/`
  directory.

Environment variables override file values; file values override defaults.
Format: `KEY=value` lines, `#` comments, no quoting semantics beyond
stripping one pair of surrounding quotes.

## Variables (Phase 1)

| Variable | Default | Meaning |
|---|---|---|
| `DEVCOORDINATOR2_SOCKET` | `/run/devcoordinator2/daemon.sock` | daemon Unix socket path |
| `DEVCOORDINATOR2_SANDBOX_BRIDGE_DIR` | `/tmp/devcoordinator2-bridge` | daemon-owned file transport used only when a client sandbox denies socket syscalls |
| `DEVCOORDINATOR2_STATE_DIR` | `/var/lib/devcoordinator2` | authority DB and runtime state |
| `DEVCOORDINATOR2_UNIT_PREFIX` | `devcoordinator2-test` | transient test unit prefix (dev instances use e.g. `devcoordinator2-dev`) |
| `DEVCOORDINATOR2_SLICE` | `devcoordinator2-tests.slice` | parent slice for test units |
| `DEVCOORDINATOR2_CLIENT_GROUP` | `devcoordinator2-clients` | Unix group granted socket access |
| `DEVCOORDINATOR2_PORT_RANGE` | `20000-29999` | host port range the daemon leases to deployment components |
| `DEVCOORDINATOR2_BASE_DOMAIN` | (empty) | base public domain; deployment `domain` labels resolve beneath it (Phase 3+) |
| `DEVCOORDINATOR2_EDGE_UID` | (unset) | Unix uid of the edge service; the only peer allowed to assert a public identity |
| `DEVCOORDINATOR2_ADMIN_EMAILS` | (empty) | comma-separated bootstrap administrators |
| `DEVCOORDINATOR2_TELEGRAM_TOKEN_FILE` | (unset) | private 0600 file holding the single server-owned bot token; unset disables notifications |
| `DEVCOORDINATOR2_TELEGRAM_API` | `https://api.telegram.org` | API base (tests point it at a fixture) |
| `DEVCOORDINATOR2_BUGS_DIR` | `/var/lib/devcoordinator2-bugs` | independent open-bug store; world-writable so any local account reports bugs (DC2-2026-08-24-OPEN-LOCAL-ACCESS) |
| `DEVCOORDINATOR2_COMPOSE_ENV_ALLOWLIST_FILE` | (unset) | absolute private schema-1 JSON file authorizing exact repository-ID/relative-path Compose interpolation files |
| `DEVCOORDINATOR2_CODEX_USAGE_SOURCES_FILE` | (unset) | absolute root-only schema-1 JSON file listing the same-owner Codex collectors that may contribute combined repository analytics |

Edge configuration lives in `/etc/devcoordinator2/edge.env` (`docs/edge.md`).

The socket's runtime directory also contains three content-free coordination
files. `test-admission.lock` closes the start-versus-upgrade race;
`test-drain.json` is a root-owned live installer lease; and
`test-activity.json` is an atomic receipt containing only active run and unit
identities. A normal source-owned installer waits on receipt replacement
events before restarting services from the verified live checkout. A dead lease is removed on the next start,
and the first upgrade from an older daemon temporarily fences its socket with a
parent-death guard so an interrupted installer restores connectivity. None of
these files is completion evidence for a check, contains a command, output,
path, credential, or caller identity, or changes REQ-TEST-08 crash recovery.

Clients that receive `EPERM` while creating or connecting a socket use the
same protocol-2 request and response envelope through the daemon-owned
`DEVCOORDINATOR2_SANDBOX_BRIDGE_DIR`. The bridge directory is root-owned and
sticky; request and response files are private (`0600`), bounded by the normal
protocol limits, and responses are owned by the request file's UID/GID. The
daemon validates the envelope before dispatch and never treats its client
identity field as authority. The Unix socket remains the first path. The
bridge does not listen on TCP or change operation authorization. Interrupted
processing files produce a bounded re-query error rather than replaying a
mutation.

## Compose environment-file authorization

Repository `env_file` declarations grant no authority by themselves. The
allowlist is `root:root` mode 0640, regular, non-symlink, at most 64 KiB, and
not group/world writable:

```json
{
  "schema": 1,
  "authorizations": [
    {"repository_id": "r0123456789abcdef", "path": "deploy/dev.env"}
  ]
}
```

Each path is normalized, relative, and exact. Runtime use additionally proves
that the current file is regular, non-symlink, stays inside the worktree, and
remains Git-ignored. Missing or malformed policy refuses daemon startup;
missing authorization refuses the repository operation. Values are never read
into Coordinator metadata or results.

Only the root daemon loads and validates this policy. Thin CLI/MCP clients load
the socket and ordinary instance values, then submit requests to the daemon;
they need neither read nor write permission on the allowlist. Application
services and their APIs never receive the policy path or authorization data.

The installer accepts repeatable
`--compose-env-authorization REPOSITORY=RELATIVE_PATH`; it resolves the Git
common-root identity, proves the existing file is ignored and safe, merges the
private allowlist atomically, and adds the instance-file pointer without
removing prior authorizations.

### Scoped live changes

With the policy location already configured, trusted local callers and existing
server administrators can update one declared repository/file authorization
without restarting the daemon or reinstalling the instance:

```sh
devcoordinator2 config show
devcoordinator2 deployment preflight /path/to/repository --name web
devcoordinator2 config authorize /path/to/repository --name web --file deploy/dev.env --expected-revision REVISION
devcoordinator2 config revoke /path/to/repository --name web --file deploy/dev.env --expected-revision REVISION
devcoordinator2 config reload --expected-revision REVISION
```

Use the current `active_revision` from `config show`. Authorize/revoke preserve
every other entry and the file's owner and mode, atomically publish the private
policy, then activate it. Trusted local agents have standing owner approval to
manage declared development-file grants within the same-owner, non-production
boundary; do not request a separate conversational approval for each file
(DC2-2026-09-07-TRUSTED-DEVELOPMENT-CONFIG). Use the exact target through these
commands rather than editing the private policy or removing its checks.
Preflight does not grant access automatically. Revocation does not stop
already-running services, but subsequent Compose file use must pass the gate.

This permission preserves credential confidentiality and does not authorize
public-access changes, production changes, destructive data changes, or bypassing
host/tool approvals. Respect explicit project restrictions and later owner
directions. A missing development-file grant is normally something the trusted
agent can resolve through `config authorize`, not a reason to stop for another
owner decision.

Reload validates the complete policy at its existing location. Invalid input
leaves the active configuration unchanged; external edits and concurrent stale
updates return `configuration_conflict` rather than silently overwriting changes.
Show returns counts, active/stored revision fingerprints, reload state, and the
latest 20 value-free change receipts; the complete receipt history stays in the
authority database. Neither values nor private policy paths are returned.

The policy location and all other instance settings still require the reviewed
installation/restart workflow. An unconfigured policy location returns
`configuration_restart_required`; live changes never create another policy
location, rewrite the instance file, or bypass a refused host approval.

The installed service retains `ProtectSystem=full` and permits writes to its
existing `/etc/devcoordinator2` directory. Atomic publication needs permission
to create and rename a private temporary file in the policy's parent directory;
making only the policy file writable is insufficient. The rest of `/etc` stays
read-only. Upgrade older service units through the reviewed installer; do not
disable filesystem protection or move credentials to bypass a publication error.

The daemon holds an exclusive lifetime lease beside its Unix socket before
opening the database or recovering jobs. Duplicate startup preserves a live
listener. Missing owned sockets are recovered through directory events without
cancelling accepted operations; foreign replacement sockets/files are preserved.

## Codex usage sources

The source policy is a root-owned mode-0600 regular non-symlink file. Each
entry explicitly binds one non-root Unix UID to that account's private
`CODEX_HOME` and installed Codex executable:

```json
{
  "schema": 1,
  "sources": [
    {
      "uid": 1000,
      "codex_home": "/home/developer/.codex",
      "executable": "/home/developer/.local/bin/codex"
    }
  ]
}
```

The daemon invokes the executable as that UID only to resolve the collector's
opaque repository key, then opens `usage/usage.sqlite3` read-only. Missing or
unsupported sources become partial coverage; they never block unrelated
Coordinator functions and never contribute zeroes. Paths, UIDs, account
aliases, and per-source values are not returned by the API.
Missing repository links are resolved only when an operator opens that
repository. Until then the collection says "Open repository to index usage";
successful links are persisted, so later collection reads and restarts stay
fast without launching unused Codex processes.

The installer accepts repeatable `--codex-usage-account UNIX_ACCOUNT` for the
default `~/.codex` and `~/.local/bin/codex` locations, merges the private policy
atomically, and preserves previously configured sources.

## Enforcement

For an owner-confirmed trusted host, `devcoordinator2-tooling install console-access
--trusted-loopback enabled` changes only the existing edge configuration. It
preserves its owner, group, permissions and other settings. The next reviewed
activation enables Console access without an OIDC session for direct loopback
requests. Disable it with the same command and `--trusted-loopback disabled`.
Forwarded or cross-origin browser requests and deployment routes keep ordinary
authentication. The option defaults to disabled; it is not a trusted LAN range.

`devcoordinator2-tooling check no-instance-data` scans committed content against the
untracked pattern list `instance/forbidden-strings.txt` and fails on any
match. Run it before every commit and in the acceptance checklist.
