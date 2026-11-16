# Native PostgreSQL/v3 delivery proof

`verify-live-tickets.mjs` keeps its default ticket journey. Its explicit
`CONSOLE_VERIFY_MODE=postgres-v3` mode proves the `postgres-v3-refactor` target
from actual accepted evidence and read-only native observations. It does not
perform finalization, delete sources, submit delivery, or complete ledger tasks.

Run the final mode only after final live formal/manual, API and MCP acceptance,
actual installer finalization, and the separately reviewed eight-file retirement.
The final executor run uses the real registered source and
`.devcoordinator/test/logs/runs/<run_id>`; do not copy run metadata into that store.
Its retained artifact must contain the helper's complete output directory.
The input run identity must match the executor-injected `DEVCOORDINATOR_RUN_ID`.

Alongside `CONSOLE_VERIFY_BASE_URL`, `CONSOLE_VERIFY_SOURCE_SHA256`, and
`CONSOLE_VERIFY_OUT`, set:

```
CONSOLE_VERIFY_MODE=postgres-v3
CONSOLE_VERIFY_CONFIG_SHA256=<accepted executor configuration digest>
CONSOLE_VERIFY_RUN_ID=<actual executor run identity>
CONSOLE_VERIFY_ACCEPTANCE=<absolute private input JSON path>
CONSOLE_VERIFY_ACCEPTANCE_SHA256=<reviewed input bytes SHA-256>
CONSOLE_VERIFY_TOOLING=<verified matching-source tooling binary>
```

Use the actual accepted configuration definition's digest, with its scope
recorded; the final plan contains derived digest bindings and is not hashed
recursively into its own `config_digest`.

Keep private originals intact. The reviewed input can name caller-readable
copies, including root-owned mode-0440 files beneath root:caller-group mode-0750
directories. Inputs must be regular non-symlink files without world access or
group/other write permission. A sanitized copy must carry its original receipt's
digest and byte count. Do not infer a passed receipt while sanitizing it.

The input is schema 1 with these fields:

- `target: "postgres-v3-refactor"`, exact registered `repository_id`, final
  `source_commit` (40 hex), `source_sha256`, `config_sha256`, `run_id`, and
  `schema_version: 35`.
- `formal`: one to eight `{report, queue, review}` batches. Every reference is
  `{path, sha256}` with an absolute path. These are the original identity-bound
  formal/manual artifacts, accessible to the proof caller. The existing
  `tooling formal-ui review` validator verifies full fresh coverage, exact
  screenshot hashes and manual decisions. Each page must observe the final
  `git:<source_commit>` binding. Do not rewrite formal paths or receipts.
- `receipts`: references named `live`, `critical-api`, `mcp`, `migration`,
  `capabilities`, `finalization`, `backup`, and `helper8`.
- `completion`: a reference to the actual schema-1 completion inventory.
- `helper8_inventory_sha256` and `helper8_binary_sha256`: the reviewed private
  inventory and executable identities, supplied in this input, never source.
- `retirement_targets`: the reviewed exact twelve `{path, sha256, group}` rows,
  four from the latest inherited finalizer list (`group: "finalizer"`) and eight
  from the reviewed helper inventory (`group: "helper8"`). No discovery is run.
- Optional `supporting`: at most 32 `{id, kind, source_commit, receipt}` rows;
  kinds are `source-test`, `isolated-runtime`, `source-size-audit`, or
  `visual-audit`. Their historical identities are retained as supporting
  provenance, never promoted to final live observations.

For a reviewed sanitized reference, also provide `original_sha256` and
`original_bytes`. The actual copied bytes must match `sha256`. Cross-receipt
bindings use the preserved original digest. The input is limited to 128 KiB;
ordinary referenced JSON to 2 MiB, formal reports to 16 MiB, manual reviews to
4 MiB, and completion to 32 KiB. Reads remain bounded if a file grows.

The receipts retain these actual source-owned results:

| Reference | Required result |
| --- | --- |
| `live` | Exact installer `LiveAcceptance` schema: passed, final daemon/edge source, four binary hashes, real timestamp, browser/edge-source/critical-journeys/critical-api/mcp-reconnect checks and evidence. |
| `critical-api` | Normalized `{schema:1,result:"passed",source_commit,protocol_version:3,checked_at_ms,checks,mutation_readbacks,unexpected_errors:0}` from actual live calls/readbacks; positive counts and timestamp no later than `live`. |
| `mcp` | Normalized `{schema:1,result:"passed",source_commit,protocol_version:3,checked_at_ms,reconnected:true,discovery:true,calls_succeeded,unexpected_errors:0}` from actual reconnected live discovery/calls. |
| `migration` | Normalized passed result with actual `source_commit`, `tables`, `rows`, `sqlite_source_sha256`, and true `all_tables_equal`, `restart_equal`, `foreign_keys_valid`, `identities_valid`. Preserve its historical source stamp. |
| `capabilities` | Final source, passed result, baseline/current catalog hashes, `baseline_operations:193`, `preserved_operations:193`, `current_operations:194`, and true `authorization_unchanged`, `cli_matches_catalog`, `mcp_matches_catalog`, `live_discovery_verified`. Static inventory supplements live observations. |
| `finalization` | Actual `FinalizationReceipt`: finalized, final source, four retired sources, exact original live-acceptance and PostgreSQL archive digests. |
| `backup` | Actual final recoverability result: verified, isolated restore removed, matched-frozen-authority false, same archive digest and positive byte count. |
| `helper8` | Existing reviewed receipt schema: retired, eight sources, expected inventory hash, and exact original finalization/live/archive/verification gate hashes. No replacement helper receipt is introduced. |

The proof caller must be able to perform all twelve nofollow absence checks.
An inaccessible ancestor refuses the proof; it never proves absence. An
explicitly reviewed root `executor run-local` execution preserves that direct
observation without changing protected-directory permissions. Normal daemon
execution-identity restrictions remain unchanged.

Product capabilities must be `real_e2e` and collectively reference all nine
live/operational summaries using `run/<run_id>/evidence/<name>.json`:
`live`, `formal`, `critical-api`, `mcp`, `migration`, `finalization`, `backup`,
`helper8`, and `retirement`. Enabled controls also reference rendered `formal`
evidence. Source catalog checks and isolated root/load tests cannot be a
product capability's sole live evidence. Retain any applicable visual audit in
the accepted supporting evidence. The existing delivery API checks linked task
states; a complete claim cannot silently omit an unfinished accepted outcome.

The helper performs `ping`, ID-scoped `plan.overview`, `decision.tail` and
`storage.inventory`, plus `test.admission.status` and `health.summary`. It never
uses path registration or invents a daemon current-run summary. It then checks
the HTTPS HTML and every served Console asset against source. Only sanitized
counts, approved metadata and hash-bound summaries are retained; private API
records, original paths, receipt notes and logs are not copied into summaries.
The formal UI digest remains separate from the governed repository/config
digests. The completion bytes are preserved exactly.

After the actual executor finishes successfully, obtain its real
`test.artifact.catalog` identities and submit `release.deliver_evidence` with
`kind: "native-console"`, `target: "postgres-v3-refactor"`,
`completion_file: "completion.json"`, and `verification_file: "delivery.json"`.
Require a qualified receipt with the intended complete claim and zero
incomplete capabilities before closing the matching delivery outcome.

The isolated regression is `node --test console/verify-live-tickets.test.mjs`,
also invoked by the existing Rust formal-UI CLI integration suite on Linux.
It uses a new loopback HTTPS fixture and the real formal-review validator;
its synthetic receipt fixtures are not live acceptance evidence.
