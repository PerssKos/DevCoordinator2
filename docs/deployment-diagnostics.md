# Deployment failure diagnostics

Trusted local agents covered by recorded standing owner authority may repair
Coordinator defects blocking approved development without another conversational
authorization. Follow `security-assumptions.md` and the repository's reviewed
non-self-hosting workflow. Mandatory host/tool controls still apply.

Deployment actions wait for their finite runtime operations rather than expiring
at the ten-second ordinary-read deadline. Closing a client does not cancel an
accepted deployment. Inspect `deployment status` before retrying; a retry is a new
attempt, not a way to recover the first client's response.

Compose build/start output is retained in the existing private build log, with
generation and component separators. Ordinary errors point to this surface rather
than embedding build output. Read only the needed bounded portion:

```sh
devcoordinator2 deployment logs --deployment-id <id> --component build --tail-lines 200
```

The log includes stdout and stderr even when the build fails, times out, or the
client disconnects. It is mode 0600 and uses the existing nofollow file boundary.
Rollback preserves the prior working generation but does not replace a new
component failure with its obsolete error. A missing component from the failed
candidate does not prevent reading the build log.

Earlier Compose output that was never retained cannot be reconstructed. After
diagnosis, an authorized retry captures fresh evidence; this does not by itself
prove the application's release scope complete.

## Docker and PostgreSQL bridge failures

Before retrying a Docker or PostgreSQL deployment, run `deployment preflight`.
The Coordinator checks Docker's `bridge` network and the host interface it is
configured to use before it creates or starts a container. If that interface is
missing, the result contains `docker_network_unavailable` and explains the
host-repair boundary. `deployment status` repeats the blocker in readiness, so
an agent can inspect a degraded deployment without guessing from an old
component error.

Do not retry `apply`, `start`, or `restart` while that blocker is present. Use
the authorized host maintenance path to restore the Docker bridge, then rerun
preflight and verify `deployment status` plus `health containers`. The
Coordinator deliberately does not recreate `docker0` or restart Docker on its
own because those actions can affect unrelated containers. Known Docker
`veth`/bridge failures from disposable test runs are reduced to the same
secret-free diagnostic; the run still records its exact cleanup result.

Decisions: `DC2-2026-09-07-TRUSTED-AGENT-REPAIR-AUTHORITY`,
`DC2-2026-09-07-RETAIN-COMPOSE-FAILURES`, and
`docker-network-preflight-20261004`.

Installer activation selects a fresh transaction directory by default. Recovery
requires the explicit unfinished transaction returned for that installation;
completed transactions and mismatched backups or installation identities are
refused before services change. Ordinary daemon or route errors are not grounds
for restoring a historical database. Installation fences cover the Unix socket
and sandbox bridge, and accepted requests drain before backup.
