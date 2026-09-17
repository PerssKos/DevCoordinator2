# Atomic Route Document Contract (edge ↔ daemon)

Defined now so Phase 1 foundations do not contradict it; implemented in
Phase 5. Informed by the proven legacy edge publication mechanism, trimmed.

## Principles

- The daemon owns desired route state and publishes it as **one complete
  JSON snapshot** — never a diff or a partial update.
- Publication is atomic: write temp file, fsync, rename into place (or the
  socket-push equivalent ending in an atomic replace on the edge side).
- The edge persists the last valid document plus a `last-known-good` copy
  and keeps serving from it across daemon restarts. A malformed, oversized,
  or partially-written document never clears currently served routes.
- Size cap 2 MiB. Integrity: `payload_sha256` over the canonical payload
  bytes; mismatch → reject, keep serving previous.
- Grants bind to immutable `deployment_id`, never to a mutable domain or
  port.

## Shape (route schema 2)

```json
{
  "schema": 2,
  "payload_sha256": "<hex>",
  "generation": 42,
  "published_at": "2026-08-22T12:00:00Z",
  "domain": "<base domain from instance configuration>",
  "routes": [
    {
      "deployment_id": "d…",
      "domain": "<fqdn>",
      "port": 12345,
      "scheme": "http",
      "auth": "public" | "authenticated",
      "lease_id": "l..."
    }
  ],
  "access": {
    "owners": ["<identity>"],
    "grants": [{"identity": "<identity>", "deployment_id": "d…", "role": "access|viewer|operator|administrator"}]
  }
}
```

- `generation` increases monotonically; the edge ignores documents with a
  generation lower than the one it serves. The daemon keeps a persisted
  high-water floor outside the recoverable database and also uses the
  publication timestamp as a lower bound.
- Managed routes include the immutable `lease_id` for their deployment and
  component. The daemon refuses to publish a route whose lease, port, owner,
  or healthy selected generation does not agree. Legacy schema-1 documents
  remain readable during migration; new publications use schema 2.
- Domains and identities are instance data: they appear only in the
  published document on the host, never in repository content.
- The edge enforces `auth` and grants per route; the daemon never handles
  public sessions.

Routed processes replace their runtime on the same lease and port; a failed
candidate restores the previous component specification and verifies its
listener. Non-routed components retain generation-scoped deployment behavior.
Observed-only routes carry `observed: true` and retain their existing native
ownership checks until they are adopted as managed components.

The edge persists an acknowledgement containing only its accepted schema,
generation and checksum in its own state directory. Withdrawal must be
acknowledged before a routed lease can be released for another deployment.
The installer compares that acknowledgement with the published document and
checks the referenced managed leases, hostnames and listeners before reporting
recovery complete. An observed-only route must still match its recorded hostname,
deployment, component and port. Its external application's availability remains
an application health result; an external outage does not roll back the
Coordinator installation or change that application's declared endpoint.
