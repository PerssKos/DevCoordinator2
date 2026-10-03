# Formal Web UI Verification

This skill runs a deterministic Playwright/Chromium heuristic over rendered
web pages. It measures DOM geometry, computed visibility, clipping, occlusion,
off-canvas controls, broken media, contrast risks, document overflow, and
visible scrollbars with their same-axis nesting chains. Before scrolling, it
also measures browser navigation TTFB and document LCP. Defaults require local
TTFB below 10 ms and LCP below 800 ms; global or per-target contracts may
prescribe other strict thresholds. Every horizontal
scrollbar is a warning; nested horizontal scrolling is blocking. Two vertical
scroll layers warn and three or more block, with the document scrollbar
counting as a layer and mixed axes kept separate. It also measures rendered
placeholders and every native select option label without retaining their text.
Visible native inputs, textareas, and selects that escape a non-scrollable
layout owner are blocking unless an active horizontal scroll path or reasoned
overlap allowance applies. The verifier also supports opt-in readable-content
inset contracts and samples immediately around declared responsive breakpoints.
It traverses discoverable open shadow roots, evaluates
Playwright-reachable frames, supports mobile device descriptors, and can open
declared interaction states with bounded actions. Top-level `requiredCoverage`
can require an exact named target, state, viewport, and optional CSS width;
missing or ambiguous cells fail coverage, so a closed or differently sized page
cannot stand in for the reported transient state. It complements screenshots
and human review; it cannot discover closed shadow roots or prove undeclared UI
states correct, and it reports reachable contexts it cannot evaluate as
coverage limits.

Every effective target/state also declares its primary journey, frequency/risk
context, semantic rendered regions, light/dark/mixed theme, and
repository-relative UI review inputs. The verifier rejects secondary workflows
above primary content, offscreen or unfocused continuation, nested horizontal
scrolling, triple vertical scroll nesting, WCAG text contrast failures, and
large declared-theme contradictions, plus applicable TTFB/LCP threshold
breaches. Palette-cohesion risks stay
explicit agent-review evidence rather than automatic aesthetic verdicts.

Each checked cell automatically captures a redacted initial viewport and full
page. `review-queue.json` contains only cells whose mapped UI inputs or
journey/theme intent changed, or whose route/state/viewport is new. Screenshot
hashes prove artifact integrity and never trigger review.

The verifier also supports fast, safe-complete execution: exact event/readback
waits, zero-wait handoff for conditionally owned controls, bounded concurrency
with resource locks, journey-priority ordering, one in-memory authentication
bootstrap per role, changed-input development selection, and an explicit
content-addressed development cache. Ordinary failures do not stop later safe
cells. Subsets and cache hits are always marked ineligible for readiness; final
delivery still requires a fresh complete run.

## Fail-closed handoff pipeline

Formal verification is the first gate. Its receipt has `formal.result` equal to
exactly one of `passed`, `failed`, `blocked`, or `incomplete`:

- `passed` means a fresh complete all-cell run, exit `0`, readiness-eligible
  coverage, all required cells and assertions satisfied, no blocking findings,
  and complete retained artifacts;
- `failed` means rendered assertions or blocking findings failed;
- `blocked` means the render path or required authentication, screenshots,
  browser/tooling, or Coordinator evidence was unavailable; and
- `incomplete` means a subset, cache hit, skipped cell, missing coverage, data
  shape, or partial evidence was used. `development-passed` is incomplete,
  never readiness `passed`.

The only valid handoff order is:

```text
formal.result == passed
    -> manual.result == passed
        -> Product Design audit (when an approved visual target exists)
            -> deployment/source-identity.result == passed
                -> qualified release.deliver_evidence receipt
```

Do not open screenshot pairs or run `$product-design:audit` after a formal
failure. Preserve the report, journey evidence, review queue, screenshots, and
diagnostic findings; repair the product and rerun the complete formal plan on a
fresh candidate. Any later gate failure blocks handoff and requires repair plus
a fresh applicable review. HTTP 200, container health, matching static assets,
or a successful deployment command never proves UI completion.

Run the self-test before relying on it:

```bash
devcoordinator2-tooling formal-ui self-test
```

The self-test resolves Playwright explicitly from the repository's locked
`ci/playwright/node_modules` installation (or
`FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES`) and passes that path to the verifier.
It does not depend on the temporary audit directory or a manually injected
`NODE_PATH`. If the locked dependency has not been installed in a checkout,
run `npm ci --ignore-scripts --prefix ci/playwright` once.

Verify explicit targets through a complete config. Bare `--url` targets fail
coverage because they have no journey/theme/input contract:

```bash
devcoordinator2-tooling formal-ui verify \
  --config formal-web-ui.json \
  --fail-on critical
```

See `references/journey_review_contract.md` for the complete target/state
schema and changed-review workflow.

Target-specific breakpoint samples, readable insets, and deployment/source
binding are configured together:

```json
{
  "repoRoot": "/absolute/path/to/repository",
  "targetDefaults": {
    "journeys": [{"id": "view-items", "frequencyPercent": 100, "risk": "normal"}],
    "primaryJourney": "view-items",
    "regions": [{"selector": ".items-card", "role": "primary-content", "journey": "view-items"}],
    "theme": "light",
    "reviewInputs": [{"path": "src/items", "kind": "ui-code"}]
  },
  "targets": [{
    "name": "items",
    "url": "http://127.0.0.1:3000/items",
    "breakpointProfile": {
      "name": "items-layout",
      "breakpoints": [768, 1024],
      "height": 900
    },
    "contentInsets": [{"selector": ".items-card", "min": 12}],
    "sourceBinding": {"expected": "git:abc123"}
  }],
  "viewports": [{"name": "desktop", "width": 1440, "height": 900}],
  "requiredCoverage": [{"target": "items", "state": "base", "viewport": "desktop", "width": 1440}],
  "execution": {"maxConcurrency": 4},
  "performance": {"ttfbMs": 10, "lcpMs": 800, "ttfbLocalOnly": true},
  "maxPageCount": 12
}
```

Each breakpoint adds `breakpoint−1`, `breakpoint`, and `breakpoint+1` for only
that target. Equivalent cells are de-duplicated. Expansion above
`maxPageCount` fails setup instead of silently sampling fewer pages. A source
binding reads `X-UI-Source-Revision` (or a configured meta name/header) from
the deployment and fails coverage when it is missing or differs from the
expected source value.

Required coverage must include every exact user-reported route/state/theme/
viewport/device/browser/auth condition, supported phone/intermediate/desktop/
wide layout, affected interaction state and theme, and every layout-changing
production data shape. Fixtures must render conditional structures such as
symmetry families and hidden navigation tracks. Each applicable cell records
geometry assertions for hidden tracks, primary-content width, heading and
canonical-identifier readability, character-by-character wrapping, clipping,
document horizontal overflow, and initial-viewport placement.

The default invocation creates a unique external artifact directory (normally
under the system temporary root), writes complete `report.json` and `report.md`
files, `journey-evidence.json`, `review-queue.json`, bounded `progress.jsonl`, and screenshot pairs, then prints one bounded JSON
receipt with the exit code, coverage, counts, directory, and filenames. Use
`--json-out` and `--markdown-out` to select known artifact paths; supplying one
derives the other. Setup/configuration failures use the same bounded receipt and
machine-readable artifact contract when a safe destination is available.
Reports record run and per-cell start/end times, verifier and privacy-safe
effective-config SHA-256 hashes, requested and final paths, every exact
route/state/viewport cell, sampled-only width coverage, and per-cell
deployment/source binding status. A sign-in redirect or stale bound deployment
does not count as checked coverage.

When the verifier runs as a governed DevCoordinator2 check, its automatic
bundle is written to the private run-leaf evidence directory. The
`journey-evidence.json` file is the Console-facing, path-free index of ordered
route/state/viewport cells and their immutable masked screenshots; it expires
with the same age/depth policy as that run's logs.

Advanced execution, ownership, authentication, changed-selection, cache, and
readiness schemas are documented in
`references/journey_review_contract.md`. Development cache storage is disabled
by default, must use an explicitly supplied existing external directory and
data revision, and never contains cookies or authentication storage state.

Full Markdown stdout is available only through the explicit human-terminal
compatibility flag `--human-readable-stdout`. Do not use that flag for agent
runs.

The formerly published `--receipt-only` flag and boolean config `receiptOnly`
remain accepted as deprecated no-op compatibility inputs. Neither changes the
safe default, and `receiptOnly: false` cannot enable full stdout.

Exit codes:

- `0`: required pages were checked and no configured finding threshold failed.
- `1`: blocking UI findings were detected.
- `2`: configuration, browser, or dependency setup failed.
- `3`: a required target could not be checked, redirected to another route,
  failed its source binding, or the minimum checked-page count was not met.

These exit codes do not replace `formal.result`: an exit `0` run with
`readinessEligible: false` is `incomplete`; setup or unavailable evidence is
`blocked`; blocking findings are `failed`; only a fresh complete readiness-
eligible run is `passed`.

Only when `formal.result == passed`, open the queue's screenshot pairs and
finalize a separate manual receipt with `devcoordinator2-tooling formal-ui
review`. That receipt must enumerate every formal target/state/theme/viewport
cell, bind screenshot identities to the formal run, and include reviewer,
decision, note, and timestamp. A pending, missing, gap, or blocked decision is
not visual completion. For mockup-backed UI, the Product Design receipt must
then include numbered journey steps, fresh screenshots, source/implementation
identity, UX/accessibility findings, evidence limits, P0–P3 classification,
iteration history, and exact `final result: passed`; Design QA alone is
insufficient. Deployment verification must bind the passed receipts to source,
artifact-manifest and image digests, deployment generation, live route, and
live rendered evidence before a qualified delivery receipt is accepted.

Explicit target failures are fail-closed. Coordinator-discovered failures can
be tolerated only with the explicit `--allow-discovered-target-failures` flag,
and remain visible in the report.

`--from-coordinator` is optional and calls the installed `devcoordinator2`
command by default. `--coordinator-command` may name another executable. The
skill consumes only typed deployment-list output and does not import, clone,
pin, build, or test Coordinator source.
