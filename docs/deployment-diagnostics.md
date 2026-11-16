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
devcoordinator2 call deployment.logs --params '{"deployment_id":"<id>","component":"build","tail_lines":200}'
```

The log includes stdout and stderr even when the build fails, times out, or the
client disconnects. It is mode 0600 and uses the existing nofollow file boundary.
Rollback preserves the prior working generation but does not replace a new
component failure with its obsolete error. A missing component from the failed
candidate does not prevent reading the build log.

Earlier Compose output that was never retained cannot be reconstructed. After
diagnosis, an authorized retry captures fresh evidence; this does not by itself
prove the application's release scope complete.

## Process startup history

Process logs retain an exact deployment, component, generation and startup
attempt identity. Read a generation first to discover its newest attempt and
the bounded retained identities:

```sh
devcoordinator2 call deployment.logs --params '{"deployment_id":"<id>","component":"api","generation":395,"tail_lines":200}'
```

Add the returned `process_log.attempt` as `attempt` to read that exact startup
again. Attempt numbers increase across the component, including recovery and
daemon restarts; they are never positional backup numbers. A systemd restart
within one submitted startup continues appending to that startup's log.

`process_log.state` is `available`, `empty`, `unavailable` or `legacy`. An empty
new startup stays empty. A missing or expired exact identity returns
`unavailable` with an empty tail; it never returns another generation or
attempt. Generation selectors remain usable after a component is removed.
Container, Compose and build logs do not support process attempt selectors.

Retention keeps eight generations and eight identified startups per generation
for each component. Starting an older generation preserves its new startup
within that same bound. The response lists retained identities and limits;
expired bytes cannot be reconstructed. Existing generation logs remain readable
with unknown attempt identity. Only a request without selectors may read an
older component-only log, whose generation and attempt are both reported as
unknown. Previously rotated legacy files retain their original names until
their generation expires; those names are not asserted to be startup identities.

Explicit clean-commit candidates run from a retained detached checkout, even
when the deployment's declared source is `worktree`. Editing the candidate
directory cannot change process restart or rollback source. Status and delivery
qualification still compare the recorded candidate origin, so later candidate
edits correctly require a new apply. Ordinary worktree applies continue using
the developer's live source directory.

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
