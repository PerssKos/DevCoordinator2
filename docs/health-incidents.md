# Health incidents

Health shows resource measurements and an optional incident inbox. The inbox
starts collapsed. Desktop opens the selected incident beside the list; phones
expand its explanation directly beneath the selected row. The explanation says
what happened, what an agent recorded doing, why the owner is needed, and the
next step. It never infers an escalation from a red deployment status.

Automatic alerts are observations. They stay out of the owner inbox until an
agent explicitly records an escalation. Successful finite work is not an
unhealthy condition. A worktree deployment can serve real users, so its source
does not classify its incidents as development work.

Trusted local callers and existing Console administrators use the same typed
operations and stored records. Existing viewer restrictions remain in place.

```sh
devcoordinator2 call health.incidents --params '{"view":"all"}'
devcoordinator2 call health.incident.update --params '{"incident_id":"INCIDENT","expected_revision":0,"status":"handling","agent_response":"Investigating the failed service."}'
devcoordinator2 schema health.incident.update
devcoordinator2 call health.incident.update --params-file /absolute/incident-response.json
```

These examples describe separate transitions, not a script to run unchanged.
Use the current incident identity and revision from the read. Record only
actions actually performed. Development observations cannot be escalated;
`handling` and `suppressed` observations never enter the owner queue.
Escalation requires the response, reason, and next step. A missing history is
not proof that no agent acted.

The API and MCP `call_operation` use `health.incidents` and
`health.incident.update`. List accepts `view`
(`attention`, `dismissed`, `all`), `limit` (1–50, default 20), and the returned
`before` cursor. `attention_count` and `dismissed_count` are independent of the
current page. Update requires `incident_id`, `expected_revision`, and `status`;
the optional text and category fields match the CLI example. Invalid or stale
updates make no change. Every accepted response has an immutable history entry.

Dismiss and Restore save through the API, then re-read the queue. A failed
save leaves the incident visible with an error. Dismissal survives reload and
another browser. It applies to the exact observed occurrence, identified by
the alert key and opening time. A new recurrence starts unreviewed; recovery
removes an active escalation while retaining its response history. Dismissing
an occurrence does not stop its service or claim that its cause was fixed.

Repositories use the existing verified Git-origin family identity to group
generated checkouts. Each checkout and deployment keeps its exact continuation.
CPU and memory can be summed across attributed records. When recursive
checkout storage overlaps, the family shows the main checkout's storage with
that label and exposes the remaining exact measurements under Checkouts.

Acceptance reuses the real Console/PostgreSQL fixture from `verify-glossary.mjs`
with `CONSOLE_VERIFY_HEALTH_ONLY=1` and the `glossary-fixture` feature. Incident
authorization, transitions, revisions and persistence run through the real
control plane. Resource charts use declared deterministic fixture readings.
The installed Console is separately checked against actual data and assets.
