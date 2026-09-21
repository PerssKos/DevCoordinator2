# Stable Edge (Phase 5)

`edge/devcoordinator2-edge.mjs` (Node.js ≥ 20, zero dependencies) owns
exactly: public TLS/listener continuity, sign-in and session validation,
per-route deployment-grant enforcement, domain→port proxying from the last
valid route document, and availability while `devcoordinatord` restarts.
It never owns lifecycle, test state, resource inventory, users as a second
authority, or notifications.

Reused from the legacy edge after review (`edge/lib/`): HMAC cookie sessions,
the OIDC authorization-code + PKCE client, self-contained auth pages, the
proxy (strips session cookies from upstream traffic, forwards the verified
identity as `x-devcoordinator2-email` / `x-devcoordinator2-route-id` on
session-authenticated routes only), and a static file server for the Console.

## Behavior

- Route document (`docs/route-document.md`): read from
  `EDGE_ROUTES_FILE` (the daemon publishes to `<state>/public/routes.json`, the only world-readable part of its state), validated (schema, checksum, shape), kept as
  `routes.last-known-good.json` in `EDGE_STATE_DIR`. Malformed, tampered, or
  older-generation documents never clear served routes. Reload on file
  change and every 5 s.
- Hosts: `<label>.<base domain>` routes to the published port; the console
  host (`console.<base domain>` by default) serves `/auth/*`, `/healthz`,
  the Console (Phase 7), and the `/api/v2/<operation>` bridge.
- Authorization per request: `auth: public` routes proxy without sign-in;
  authenticated routes require a session whose identity is an owner or
  holds any grant for that exact `deployment_id`. Revocation is effective on
  the next request because the daemon republishes the document on every
  user/grant change. An explicitly enabled local agent may also reach an
  authenticated deployment route when the request is a direct loopback
  request carrying `X-DevCoordinator2-Agent: 1`; this does not apply to the
  Console API or to requests with forwarding or cross-origin browser headers.
- Sign-in: `/auth/start` → identity provider → `/auth/callback` on the
  console host; the edge then asks the daemon to admit the identity
  (`user.accept_invitation`, trusted only because the edge's Unix uid is the
  configured `DEVCOORDINATOR2_EDGE_UID`). A session is issued regardless;
  what it may reach is decided by the route document and the daemon.
- `/api/v2/<operation>` (POST JSON params, session required): forwarded to the
  daemon with `client.identity = <signed-in e-mail>`; the daemon applies
  roles (`docs/contract-commands.md`). Operation names must match
  dot-separated operation grammar such as `family.name` or
  `family.area.action` (lowercase, `[a-z_]` after each dot) or be exactly
  `ping` — anything else is refused at the edge and never reaches the
  daemon. Public callers address deployments
  by `deployment_id`; actions on their behalf execute as the account that
  created the deployment.
- The former `/api/<operation>` route returns a bounded protocol-2
  `protocol_unsupported` error and never forwards the request.
- Protected deployment routes preserve challenge-free upstream `401` responses,
  including empty session-check responses and JSON sign-in errors. Applications
  may keep their own cookie sessions behind edge sign-in. A `401` carrying a
  `WWW-Authenticate` challenge still becomes a bounded gateway error, preventing
  another browser HTTP-auth prompt. Edge authorization and cookie isolation apply
  on both response paths; an application cookie cannot grant edge access.

## Local agent access to authenticated deployments

Set `EDGE_TRUST_LOCAL_AGENT=1` in the edge's private instance environment when
trusted local agents must test a deployment whose route document says
`auth: authenticated`. An agent sends the exact marker header while connecting
directly to the edge from the host:

```sh
curl --resolve app.example.test:443:127.0.0.1 \
  -H 'X-DevCoordinator2-Agent: 1' \
  https://app.example.test/health
```

The marker is a local-trust signal, not an encrypted secret. The edge accepts it
only from a kernel-reported IPv4/IPv6 loopback peer, with no `Forwarded` or
`X-Forwarded-*` headers and no cross-origin browser metadata. It strips the
marker and the edge session cookie before proxying, does not invent a user
identity, and leaves any authentication implemented by the deployment itself
in place. The setting defaults to disabled; public and forwarded clients still
use ordinary OIDC sessions and deployment grants.

## Configuration (instance data, never in the repository)

| Variable | Meaning |
|---|---|
| `EDGE_BASE_DOMAIN` | base public domain (required) |
| `EDGE_CONSOLE_HOST` | default `console.<base>` |
| `EDGE_HTTP_PORT` / `EDGE_HTTPS_PORT` | listeners (80/443; http-only canary default 8080) |
| `EDGE_HTTP_ONLY=1` | plain HTTP listener only, insecure cookies — canary/tests |
| `EDGE_TLS_CERT` / `EDGE_TLS_KEY` | PEM paths (systemd credentials) |
| `EDGE_SESSION_SECRET_FILE` | ≥ 16 bytes |
| `EDGE_OIDC_ISSUER` | default Google; any spec-compliant issuer |
| `EDGE_OIDC_CLIENT_ID_FILE` / `EDGE_OIDC_CLIENT_SECRET_FILE` | credentials |
| `EDGE_ROUTES_FILE` | daemon route document (default instance state dir) |
| `EDGE_STATE_DIR` | last-known-good copy |
| `EDGE_DAEMON_SOCKET` | daemon socket (world-connectable since DC2-2026-08-24-OPEN-LOCAL-ACCESS; no group membership needed) |
| `EDGE_CONSOLE_DIR` | Console static assets (`console/` in the release) |
| `EDGE_TRUST_LOCAL_AGENT` | `0` (default); allow the exact local agent marker to bypass edge sign-in for authenticated deployment routes |

Daemon side: `DEVCOORDINATOR2_EDGE_UID` (the edge service uid) and
`DEVCOORDINATOR2_ADMIN_EMAILS` (bootstrap administrators).

## Tests

`node --test edge/test` runs the edge against a fixture OIDC issuer, a fake
daemon socket, and real upstreams. The Rust control tests in
`rust/control/src/access.rs` and `rust/control/src/daemon.rs` prove identity
trust, roles, invitation admission, revocation, and non-edge spoof rejection at
the daemon boundary.
