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
| `EDGE_BASE_REDIRECT=1` | optional 301 redirect from the exact base domain to the console origin, preserving path/query; off by default |
| `EDGE_CONSOLE_HOST` | default `console.<base>` |
| `EDGE_HTTP_PORT` / `EDGE_HTTPS_PORT` | listeners (80/443; http-only canary default 8080) |
| `EDGE_HTTP_ONLY=1` | plain HTTP listener only, insecure cookies — canary/tests |
| `EDGE_LISTEN_HOST` | optional listener bind address; set `127.0.0.1` for a private local canary; omitted preserves the production listener default |
| `EDGE_TLS_CERT` / `EDGE_TLS_KEY` | PEM paths (systemd credentials) |
| `EDGE_SESSION_SECRET_FILE` | ≥ 16 bytes |
| `EDGE_OIDC_ISSUER` | default Google; any spec-compliant issuer |
| `EDGE_OIDC_CLIENT_ID_FILE` / `EDGE_OIDC_CLIENT_SECRET_FILE` | credentials |
| `EDGE_ROUTES_FILE` | daemon route document (default instance state dir) |
| `EDGE_UPSTREAM_AUTH_FILE` | optional private JSON file mapping route labels to upstream Authorization headers |
| `EDGE_REVIEW_IDENTITY_FILE` | optional private policy for an existing request-bound review identity integration |
| `EDGE_ACME_WEBROOT` | optional existing certificate-renewal webroot; HTTP serves only its `.well-known/acme-challenge/<token>` files |
| `EDGE_STATE_DIR` | last-known-good copy |
| `EDGE_DAEMON_SOCKET` | daemon socket (world-connectable since DC2-2026-08-24-OPEN-LOCAL-ACCESS; no group membership needed) |
| `EDGE_CONSOLE_DIR` | Console static assets (`console/` in the release) |
| `EDGE_TRUST_LOCAL_AGENT` | `0` (default); allow the exact local agent marker to bypass edge sign-in for authenticated deployment routes |

Daemon side: `DEVCOORDINATOR2_EDGE_UID` (the edge service uid) and
`DEVCOORDINATOR2_ADMIN_EMAILS` (bootstrap administrators).

For an authenticated application that already requires an upstream credential,
set `EDGE_UPSTREAM_AUTH_FILE` to a private instance file or systemd credential
readable by the edge service, outside the repository. Its shape is
`{"schema":1,"routes":{"app":"<existing upstream Authorization value>"}}`.
Use the existing private credential storage permissions; never put these values
in the public route document, source, or ordinary logs. The file is read at edge
startup; restart the edge after a change. An unreadable or invalid configured
file prevents startup with a content-free error. Omitting it preserves current
behavior.

After normal session and deployment-grant checks, the edge replaces the caller's
Authorization header with the value for that exact authenticated route label,
for both HTTP and WebSocket requests. Unmapped protected routes receive no
Authorization header; public routes never receive a configured private value
and retain their existing caller-header behavior. Remove the mapping before
reassigning a route label to a different application. These boundaries preserve
existing access and upstream authentication during migration, as recorded in
`security-assumptions.md` under “Existing upstream credential migration”.

## Existing review identity integration

Applications that already verify `X-Spectre-Review-Identity` can retain their
existing review authorship through `EDGE_REVIEW_IDENTITY_FILE`. This optional
compatibility policy is loaded once at startup and does not change deployment
grants. It follows the confirmed same-owner migration boundary and
`DC2-MIGRATION-20260907`: preserve existing online access and credentials without
making applications public or granting additional authority.

The private JSON shape is:

```json
{
  "schema": 1,
  "issuer": "<existing assertion issuer>",
  "identity_provider_issuer": "<existing OIDC issuer>",
  "kid": "<existing signing key identifier>",
  "private_key_file": "<absolute private Ed25519 PEM credential path>",
  "routes": [{
    "label": "<exact route label>",
    "deployment_id": "<exact deployment identifier>",
    "component": "<exact component>",
    "audience": "<existing route:instance audience>"
  }]
}
```

Both files must be private regular files owned by root or the edge service;
symlinks and hard links are refused. Ordinary private files must have no
group/world permissions. Direct files in the process's systemd
`CREDENTIALS_DIRECTORY` may use its read-only service ACL: the ACL mask can
appear as group-read (`0440`) in the file mode even when `group::---` grants
the owning group no access. Such credential files must have no write or world
permission bits. Other locations do not inherit that exception. Keep these files
outside the repository, for example in systemd credentials. Invalid policy or
keys stop startup with a content-free error. The configured identity-provider
issuer must equal `EDGE_OIDC_ISSUER`; keep the existing immutable session
subject, signer issuer, key identifier and audience to preserve upstream
identity and editor bindings. The existing receiver's public-key trust remains
the authority. Restart the edge after changing this private configuration.

After current session and deployment-grant checks, the edge signs a fresh
Ed25519 assertion only for an exact authenticated label/deployment/component
binding and an exact review endpoint: context, whoami, reviews, review messages,
positions, decisions, review-media upload/content, or review export. Assertions
include the HTTP method, unchanged raw path/query, a 30-second expiry and a
unique nonce. The receiver enforces that binding and single-use lifetime. No
role is asserted: the receiver retains its existing review permission rules.

Every caller-supplied `X-Spectre-Review-Identity` is stripped, including duplicate
headers and `Connection` nominations. A fresh server assertion is added after
hop-by-hop filtering. Public routes, unconfigured or reassigned bindings,
non-review paths and all WebSocket upgrades receive no assertion. Session
cookies, keys and assertions never enter route documents or normal logs.

## Existing certificate renewal

When retaining an existing ACME HTTP-01 renewal setup, set `EDGE_ACME_WEBROOT`
to its existing webroot and grant the edge read/traverse access to the challenge
directory. The edge does not request certificates, change renewal configuration,
or expose any other webroot file. GET and HEAD challenge requests on HTTP are
handled before the normal HTTPS redirect, only for hostnames covered by the
loaded certificate, including covered names without a current deployment route.
In HTTP-only canaries, the base domain, console host, and current route hosts are
accepted instead. Other hosts, invalid tokens, non-GET/HEAD methods, missing
files, and symlink escapes receive empty 404 responses. Ordinary paths keep
their existing behavior. The setting is optional and read at startup; newly
added certificate names must already be covered before this renewal-only
handler serves their challenges.

## Tests

`node --test edge/test/*.test.mjs` runs the edge against a fixture OIDC issuer, a fake
daemon socket, and real upstreams. The Rust control tests in
`rust/control/src/access.rs` and `rust/control/src/daemon.rs` prove identity
trust, roles, invitation admission, revocation, and non-edge spoof rejection at
the daemon boundary.
