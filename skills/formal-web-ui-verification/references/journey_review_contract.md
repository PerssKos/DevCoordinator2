# Journey, Theme, And Changed-Review Contract

Use this reference when preparing a formal Web UI verifier configuration or
finalizing its visual review. Every target—including login, utility, and
component-demo targets—needs a complete contract. A bare `--url` intentionally
fails target coverage.

## Complete Target Example

```json
{
  "repoRoot": "/absolute/path/to/repository",
  "targets": [{
    "name": "accounts",
    "url": "http://127.0.0.1:3000/accounts",
    "journeys": [
      {
        "id": "view-accounts",
        "name": "View and manage accounts",
        "frequencyPercent": 99,
        "risk": "normal",
        "rationale": "Normal destination use"
      },
      {
        "id": "add-account",
        "name": "Add an account",
        "frequencyPercent": 1,
        "risk": "normal",
        "rationale": "Occasional creation"
      }
    ],
    "primaryJourney": "view-accounts",
    "regions": [
      {
        "name": "Account collection",
        "selector": "[data-ui-region='accounts']",
        "role": "primary-content",
        "journey": "view-accounts"
      },
      {
        "name": "Compact account tools",
        "selector": "[data-ui-region='account-tools']",
        "role": "supporting"
      }
    ],
    "theme": "light",
    "reviewInputs": [
      {"path": "src/accounts", "kind": "ui-code"},
      {"path": "src/styles/accounts.css", "kind": "style"},
      {"path": "src/design/tokens.css", "kind": "tokens"},
      {"path": "public/account-icons", "kind": "asset"}
    ],
    "states": [{
      "name": "add-account-open",
      "actions": [{"action": "click", "selector": "[data-action='add-account']"}],
      "primaryJourney": "add-account",
      "priorityOverrideReason": "The user explicitly activated account creation",
      "regions": [{
        "name": "Add account dialog",
        "selector": "[role='dialog'][data-purpose='add-account']",
        "role": "primary-content",
        "journey": "add-account"
      }],
      "continuation": {
        "kind": "in-page",
        "anchor": "[role='dialog'][data-purpose='add-account'] h2",
        "focusWithin": "[role='dialog'][data-purpose='add-account']",
        "maxScrollDelta": 8
      }
    }]
  }],
  "viewports": [
    {"name": "mobile", "width": 390, "height": 844},
    {"name": "desktop", "width": 1440, "height": 900}
  ],
  "requiredCoverage": [{"target": "accounts", "state": "add-account-open", "viewport": "mobile", "width": 390}],
  "performance": {"ttfbMs": 10, "lcpMs": 800, "ttfbLocalOnly": true}
}
```

`targetDefaults` may provide the same fields for coordinator-discovered or
repeated fixture targets, but every effective target/state still must resolve a
complete contract.

## Required Coverage Cells

Top-level `requiredCoverage` declares cells that must exist in the full
target × state × viewport plan. Each entry contains:

- `target`: the target's exact unadorned `name`;
- `state`: the exact state name, or `base` for the closed/default page;
- `viewport`: the exact configured or generated viewport name;
- `width`: an optional positive CSS-pixel width when the defect or breakpoint
  is tied to an exact reported size.

Each declaration must resolve to exactly one plan cell. No match is `missing`;
multiple same-name matches are `ambiguous`. Either result fails target
coverage with exit `3` even when every executed page otherwise passes. The
report retains the requirement, status, privacy-safe reason, and matching cell
IDs. Mapping is evaluated against the full declared plan before development
selection; a development subset remains ineligible for readiness.

Declare transient menus, dialogs, sheets, expanders, validation states, and
other hidden-by-default surfaces explicitly. The verifier never infers an open
state from the presence of its closed trigger or from text in an unexecuted DOM
branch.

## Exact browser, layout, data-shape, and geometry coverage

Every user-reported browser condition is a required cell. Add
`reportedBrowserStates` when a report names an exact route, state, theme,
viewport, device or user agent, authentication condition, zoom, or width/height:

```json
{
  "reportedBrowserStates": [{
    "id": "registry-compact-chromium",
    "target": "registry",
    "state": "base",
    "theme": "light",
    "viewport": {"name": "reported-1024", "width": 1024, "height": 768},
    "device": "desktop",
    "userAgent": "reported-browser",
    "auth": "signed-in",
    "zoom": 1
  }]
}
```

Each entry must resolve to exactly one `requiredCoverage` cell with the same
route, state, theme, viewport dimensions, device/browser condition, and auth
condition. A different viewport name, a narrow desktop substitute for a phone,
or a default state substituted for the reported interaction is missing
coverage. In addition to exact reported cells, every supported layout class
(`phone`, `intermediate`, `desktop`, and `wide`) and every affected theme and
interaction state must be represented. Every declared breakpoint still expands
to width−1, width, and width+1; those generated cells are mandatory, not
optional samples.

Declare every layout-changing production data shape for each affected target:

```json
{
  "fixtureDataShapes": [{
    "id": "symmetry-family",
    "revision": "fixture-2026-09-01",
    "route": "/registry",
    "state": "base",
    "conditionalDom": [
      "[data-ui-region='explorer-grid']",
      "[data-ui-track='hidden-navigation']"
    ],
    "layoutEffect": "activates the hidden explorer grid track"
  }]
}
```

The route fixture must actually render each declared shape and conditional DOM
structure. An empty, simplified, or mocked response that omits a symmetry
family, hidden navigation track, or other higher-specificity layout branch is
not evidence for that shape. Missing shape declarations or unrendered fixture
branches make the formal result `incomplete`.

Each affected target/state must also declare `geometryAssertions`. Assertions
are stable, privacy-safe IDs with selectors or region bindings and measured
results for every applicable cell. The required assertion kinds are:

- `hidden-navigation-track`: prove hidden tracks do not consume unintended
  primary-content width or create a collapsed one-character column;
- `primary-content-width`: record the rendered width and enforce the declared
  minimum or viewport-relative bound;
- `readable-heading` and `readable-canonical-identifier`: prove visibility,
  usable width, no clipping, and recognizable text layout;
- `no-character-wrapping`: fail when a heading or canonical identifier wraps
  character by character rather than within the intended text measure;
- `document-horizontal-overflow`: record document `scrollWidth` and
  `clientWidth`, failing unexplained overflow;
- `initial-viewport-placement`: prove primary content intersects the initial
  viewport before scrolling; and
- `clipping`: record self and ancestor clipping for the primary content and
  named readable text.

An assertion without a measured result, a selector that cannot resolve, or a
data shape without its applicable assertion cells is incomplete evidence. A
formal pass requires every required assertion to pass or have an explicit,
reviewed allowance retained in the report.

The executable geometry schema is a target/state `geometryAssertions` array.
Every row has a unique `id`, a `kind` from the list above, and exactly one
`selector` or `region` (the exact name of a declared region). Each effective cell
requires measured width, heading, canonical-identifier, wrapping, document
overflow, initial-placement and clipping assertions. Add the hidden-track
assertion when that layout contains a hidden navigation track.

```json
{
  "geometryAssertions": [
    {"id":"content-width","kind":"primary-content-width","selector":"main","minWidthRatio":0.75},
    {"id":"hidden-nav","kind":"hidden-navigation-track","selector":"#nav",
     "primarySelector":"main","track":{"selector":"#layout","axis":"columns","index":0},
     "maxReservedSize":0}
  ]
}
```

`minWidth` is a finite nonnegative pixel bound; `minWidthRatio` is a finite
nonnegative ratio of the viewport width. At least one is required for width
assertions. A hidden-track assertion also requires a width assertion resolving
to the same actual primary element. It measures the resolved computed grid
track, including a remaining column when its navigation element is
`display:none`. `rows` measures height and `columns` measures width;
`maxReservedSize` is an explicit finite pixel bound. Flex or unresolved tracks
are incomplete evidence. Initial placement remains a separate assertion.

Optional `allowance: {"reason":"..."}` retains a measured intentional
difference. It cannot excuse a missing selector, region, track or measurement.
Wrapping uses rendered text ranges grouped by line and grapheme counts without
retaining text. Single-character labels are a guard; intentional stacked or
CJK text requires an explicit allowance when it meets the detector condition.

Every `fixtureDataShapes` row requires `id`, `revision`, `route`, `state`,
nonempty `conditionalDom`, and `layoutEffect`. `route` includes the path and
query. Optional `target` resolves an exact target name and is required when
route/state alone matches multiple target groups. Each declared selector is
measured for attached and visible element counts. Attached-hidden tracks are
valid only with the applicable geometry measurements; missing/ambiguous cells
or unrendered shape branches are incomplete. Metadata alone never proves a
shape.

`reportedBrowserStates` matches an exact required cell and actual context.
`device` is `desktop` or the exact Playwright descriptor; optional `engine`
matches the measured renderer name, such as `chromium`. A mobile descriptor is
an emulated context: its Safari-like user agent does not establish native
Safari/WebKit or physical-phone coverage. Actual renderer/version, user agent,
mobile/touch context and device pixel ratio remain separate evidence.
`auth` identifies the actual named in-memory profile, or an anonymous fresh
context without supplied credentials. Unknown conditions remain incomplete.
Only requested browser `zoom: 1` is supported by the fresh isolated-context
default guarantee. Other browser zoom requests are incomplete; device pixel
ratio, CSS zoom and visual-viewport scale are not substitutes for browser zoom.

## Journey And Region Rules

- Journey IDs are stable. Each definition includes `frequencyPercent` and
  `risk` (`critical`, `high`, `normal`, or `low`).
- Exactly one `primaryJourney` owns each target/state. If it is not the
  highest-frequency journey, provide `priorityOverrideReason`; rare urgent work
  can legitimately be primary, but the decision must be explicit.
- `primary-content` is the named destination object or current task.
- `workflow-surface` is another journey such as create, edit, configuration,
  or administration. A lower-priority workflow before primary content fails.
- `supporting` is a compact title, search, filter, sort, or toolbar. When it
  consumes more than a compact share before primary content, it fails.
- `blocking-alert` may precede primary content only with a non-empty reason.
- Product/journey documentation defines these semantic roles. The verifier
  configuration maps them to exact selectors; do not put CSS selectors into
  product intent merely to satisfy the tool.

## Continuation Rules

Every state containing an activating click, press, check, uncheck, selection, or file upload
declares `continuation`.

- `in-page`: the anchor must be visible in the user's current viewport, focus
  must be inside `focusWithin`, and document movement must stay within
  `maxScrollDelta` (8 CSS pixels by default). A modal, mobile sheet, or nearby
  expansion can pass; a form appended below a long collection fails.
- The anchor is the revealed heading or first field. A custom component may
  mark an equivalent recognizable element with `data-ui-continuation-anchor`;
  using a broad container merely because it intersects the viewport fails.
- `navigation`: set `expectedPath`. The destination must stay on the expected
  origin and render the declared anchor in its initial viewport. Focus is not
  required merely because a new document loaded.
- `triggerActionIndex` identifies the action immediately before which the
  verifier records the user's current scroll position. It defaults to the last
  action.

## File Selection

A target state can exercise a real native file input with:

```json
{"action":"setInputFiles","selector":"input[type=file]","value":"tests/fixtures/measurements.xlsx"}
```

Declare `repoRoot` explicitly. `value` is exactly one slash-separated path
relative to that root, at most 1,024 characters. Absolute paths, backslashes,
empty/dot/traversal segments, `.git` or `.devcoordinator` segments, symlinks in
any component, missing files, directories, special files, arrays and inline
payload objects are rejected. An empty regular file is valid for testing an
application's empty-upload error. The file limit is 16 MiB.

The verifier reads the bounded regular file without following links and passes
its basename and bytes to Playwright. Common file types, including XLSX and CSV,
receive their MIME type; other extensions use `application/octet-stream`.
Hidden native inputs behind custom upload controls are supported. No arbitrary
browser code is accepted. File selection is an activating action
and requires `continuation`; use `waitFor` for the application's parsed result.
Authentication-profile actions do not support file uploads.

A digest of the fixture path and contents enters the state's review fingerprint.
The path itself is not retained in that evidence. The file is checked
again immediately before upload, and a changed fixture fails instead of using
stale planning evidence. Reports contain neither the declared path nor the
bytes. Action-derived screenshot masks cover the path and basename; callers
still declare masks for any sensitive file content rendered by the application,
as they do for other fixture data. Existing permissions and safe-target rules
apply to uploading files and to any downstream application effects.

## Conditional Control Ownership

A conditional action may declare both `ownerJourney` and `ownerState`:

```json
{
  "action": "click",
  "selector": "[data-action='advanced-target']",
  "ownerJourney": "advanced-targeting",
  "ownerState": "advanced-targeting-open"
}
```

The named owner must resolve to exactly one configured state in the same target
group, and that state's primary journey must match. Outside its owner, an absent
or hidden control records an immediate zero-wait handoff. If it is visible
outside the owner, the ownership contract is contradictory and fails. In the
owner state, the normal action and continuation contracts apply. Do not label a
control as conditionally owned merely to avoid a legitimate readiness wait.

## Observable Readiness

### Native HTTP error pages

A target may set `expectedHttpStatus` to one integer from 400 through 599 when
its user journey is the application's actual HTML recovery page. Set
`waitFor.selector` to that view's recognizable element, for example:

```json
{
  "name": "download-unavailable",
  "url": "http://127.0.0.1:3000/files/example",
  "expectedHttpStatus": 503,
  "waitFor": {"selector": ".file-error-card"}
}
```

This is only the status/readiness part of a target: it still needs the ordinary
journey, theme, regions, UI inputs, exact source binding and required geometry
and coverage declarations. Do not replace the application's error with HTTP
200 to obtain verification. The observed and expected statuses are retained;
a different status, non-HTML body, unexpected redirect, stale source or missing
view does not pass. Matching status does not exempt layout, contrast,
performance, screenshots or interaction checks. The status expectation enters
the review-intent fingerprint so a changed expectation cannot reuse an old
manual decision. Arrays, status ranges, strings, global defaults and state
overrides are rejected. Targets without this explicit declaration still refuse
HTTP errors.

`waitFor` may combine these exact signals:

- `selector`, optionally raced against `errorSelector`;
- `responseUrl` or `url` (armed before the triggering action);
- `loadState` or an explicit `networkIdleMs` deadline;
- `readback: {url, status, jsonPath?, equals?, intervalMs?}`;
- `renderFrames: 1|2`;
- compatibility `settleMs` only from 0 through 100 ms.

`timeoutMs`, `loadStateTimeoutMs`, and `networkIdleMs` are event failure
ceilings and may exceed 100 ms. `settleMs`, `pollIntervalMs`, and readback
`intervalMs` are deliberate intervals and may never exceed 100 ms. An empty
wait contract advances after two animation frames rather than an arbitrary
sleep. `afterFailureWaitFor` may collect one bounded downstream observation
after an ordinary interaction failure.

## Safe Complete Execution

Top-level `execution.maxConcurrency` is 4 by default. Each target/state may
declare:

```json
{
  "execution": {
    "parallelSafe": true,
    "resourceLocks": ["fixture-account-42"],
    "priority": 50000,
    "stopOnFailure": true,
    "stopReason": "A failed mutation can invalidate shared fixture state"
  }
}
```

Undeclared work is exclusive. Parallel-safe cells may overlap only when their
resource locks do not conflict. A fresh browser context isolates each cell, but
does not prove that shared server data is isolated. Results retain declared
plan order plus separate execution indices. Explicit priority overrides the
default risk-plus-frequency score and changes start order only.

Use `stopOnFailure` only when a failure can corrupt shared state or invalidate
later evidence, and always provide `stopReason`. It is not a fail-fast shortcut:
undeclared ordinary failures continue. A declared unsafe failure names every
remaining unexecuted cell.

Ordinary navigation, interaction, focus, assertion, page, or locale failures
become cell results while later safe cells continue. Cleanup runs for every
created context. Loss of browser authority is an unsafe stop: remaining cells
are recorded as unexecuted rather than silently omitted.

## In-Memory Authentication Profiles

```json
{
  "authProfiles": [{
    "name": "admin",
    "url": "http://127.0.0.1:3000/sign-in",
    "actions": [
      {"action": "fill", "selector": "#password", "value": "secret supplied by caller"},
      {"action": "click", "selector": "#sign-in"}
    ],
    "waitFor": {"responseUrl": "**/session", "selector": "[data-auth-ready]"}
  }],
  "targets": [{"url": "http://127.0.0.1:3000/admin", "authProfile": "admin"}]
}
```

Each profile bootstraps once. Its Playwright storage state remains in memory and
seeds a fresh context for every bound cell. Profile failure affects only bound
cells. Action values and storage contents never enter config evidence, reports,
logs, progress, or cache entries.

## Development Selection And Cache

Fast affected-cell runs are explicit development evidence:

```json
{
  "development": {
    "changedPaths": ["src/accounts/AccountList.tsx"],
    "cache": {
      "directory": "/explicit/external/formal-ui-cache",
      "dataRevision": "fixture-snapshot-2026-09-01",
      "mode": "read-write"
    }
  }
}
```

Changed paths are repository-relative and match declared `reviewInputs`. All
states and viewports for an affected target group are selected. Any unmapped
path expands to the full plan instead of risking a false subset. The full
declared plan still must fit `maxPageCount` before selection.

The cache directory must already exist, be absolute, external to `repoRoot`,
and contain no symlinked path components. Cache reuse additionally requires an
expected source binding and valid review-input fingerprint. Keys bind verifier,
browser, privacy-safe config, secret-value digest, source, intent, data
revision, route, state, and viewport. Only successful cells with matched source
identity and complete masked screenshot evidence are written atomically.
Corrupt or symlinked entries are rejected and rerun. The cache is never a
hidden baseline and never makes a run readiness-eligible.

## Native Control Geometry

- Empty input/textarea placeholders and every native `select` option label are
  measured against the closed control's usable text width, including the native
  affordance reserve. A short selected value does not hide a longer option-set
  failure.
- Measurement evidence contains only control kind, option count, and geometry.
  Option labels, placeholders, and entered values are never retained.
- Visible native inputs, textareas, and selects must stay inside the inner
  border edge of each non-scrollable layout ancestor. The nearest escape raises
  `control-outside-container`.
- An active horizontal `auto`/`scroll` path makes the control reachable and
  suppresses the containment failure, but the normal `horizontal-scrollbar`
  warning remains. Use `allowOverlap` or `data-ui-allow-overlap="reason"`
  only for a deliberate escape; it remains visible as `allowed-overlap`.

## Theme And Palette Rules

- Every target/state declares `light`, `dark`, or `mixed` theme intent.
- WCAG 2.2 AA text thresholds are critical: 4.5:1 for normal text and 3:1 for
  rendered large text. Use `allowContrast` or
  `data-ui-allow-contrast="reason"` only for a documented inactive,
  incidental, decorative, or logo exception.
- A 24×24 visible-surface sample blocks a large contradiction of declared
  light/dark intent. Use `themeExceptions` or
  `data-ui-theme-exception="reason"` for an intentional local exception.
- High-chroma surface coverage, four or more prominent accent-hue clusters,
  gradients, media, and unmeasurable compositing are warnings for screenshot
  review. They are not automatic claims that a palette is ugly.

## Rendered Navigation Performance

The verifier installs a buffered Largest Contentful Paint observer before each
document navigation. After the declared readiness contract and document load
boundary, it lets visible images decode and waits for paint/observer delivery,
then captures performance before the verifier's own full-page scroll.

Defaults are strict:

- local `localhost`, `*.localhost`, `127.0.0.1`, or `::1` navigation TTFB must
  be `< 10 ms`;
- final main-document LCP must be `< 800 ms`.

Equality fails. TTFB is `PerformanceNavigationTiming.responseStart -
requestStart`; LCP is the last buffered `LargestContentfulPaint.startTime` for
the final main document. This LCP does not measure the latency of an in-page
interaction after user input.

Set global thresholds or override them for a target/state:

```json
{
  "performance": {"ttfbMs": 10, "lcpMs": 800, "ttfbLocalOnly": true},
  "targets": [{
    "url": "http://127.0.0.1:3000/report",
    "performance": {"ttfbMs": 8, "lcpMs": 650}
  }]
}
```

`ttfbLocalOnly` defaults to `true`; set it to `false` when an explicit TTFB
threshold must also govern preview or remote targets. Non-local TTFB under the
default scope is reported as `not-applicable`, not passed. A required metric
that Chromium cannot expose raises `performance-metric-unavailable`. Evidence
contains values, thresholds, comparison, status, navigation type, LCP size,
local-target flag, and timing source only—never the LCP element, its text, or a
resource URL.

## Initial Readiness Diagnostics

`targetDefaults.initialReadinessDiagnostics` and a target override support this
optional shape. Omission or `{ "enabled": false }` disables it. A target override
replaces the defaults; resource/marker arrays are not concatenated.

```json
{
  "initialReadinessDiagnostics": {
    "enabled": true,
    "resources": [{ "id": "session", "pathname": "/api/session" }],
    "markers": [
      { "id": "login", "selector": "#login-form" },
      { "id": "ready", "selector": ".result-row" }
    ]
  }
}
```

- Unknown keys and invalid shapes fail configuration. IDs are unique within
  each list, non-sensitive lowercase ASCII aliases matching
  `[a-z][a-z0-9-]{0,31}`. Each list has at most 16 entries. A resource pathname
  is an exact same-origin path, without origin, query, fragment or credentials;
  a marker is a standard CSS selector. Both strings are bounded to 256 characters.
  Marker CSS syntax is checked in the blank verification page before navigation;
  invalid syntax fails the cell without navigating it.
- Passive listeners attach to the actual cell page before navigation. Matched
  requests retain alias, method/resource-type enums, numeric response status,
  event sequence and relative time only. Matched `request-failed` events also
  retain `failureCode`: an exact allowlisted Chromium `net::ERR_*` value without
  the `net::` prefix, or `unknown`. Only these codes are retained:
  `ERR_ABORTED`, `ERR_FAILED`, `ERR_CONNECTION_CLOSED`, `ERR_CONNECTION_RESET`,
  `ERR_CONNECTION_REFUSED`, `ERR_CONNECTION_ABORTED`, `ERR_CONNECTION_TIMED_OUT`,
  `ERR_TIMED_OUT`, `ERR_INTERNET_DISCONNECTED`, `ERR_NAME_NOT_RESOLVED`,
  `ERR_NETWORK_CHANGED`, `ERR_ADDRESS_UNREACHABLE`, `ERR_EMPTY_RESPONSE`,
  `ERR_INVALID_RESPONSE`, `ERR_INVALID_HTTP_RESPONSE`, `ERR_CONTENT_LENGTH_MISMATCH`,
  `ERR_INCOMPLETE_CHUNKED_ENCODING`, `ERR_HTTP2_PROTOCOL_ERROR`,
  `ERR_HTTP2_SERVER_REFUSED_STREAM`, `ERR_QUIC_PROTOCOL_ERROR`, `ERR_SSL_PROTOCOL_ERROR`,
  `ERR_CERT_DATE_INVALID`, `ERR_CERT_AUTHORITY_INVALID`, `ERR_CERT_COMMON_NAME_INVALID`,
  `ERR_BLOCKED_BY_CLIENT`, `ERR_BLOCKED_BY_RESPONSE`, `ERR_INSUFFICIENT_RESOURCES`,
  `ERR_HTTP_RESPONSE_CODE_FAILURE`, `ERR_RESPONSE_HEADERS_MULTIPLE_CONTENT_LENGTH`.
  Unknown codes, missing values, surrounding prose and values containing URLs
  become `unknown`; raw error text is never retained. Completed requests never
  carry `failureCode`. Unmatched resources are counted by type only.
  Page errors retain a fixed error-name class, never a message/stack. No bodies,
  headers, cookies, storage, console text, raw resource URLs or control/DOM values
  enter diagnostics. Caller aliases must not contain sensitive facts. Public
  configuration describes aliases only, not the resource paths/marker selectors.
- The per-cell object is bounded to 64 events and 16 KiB. Dropped counts are
  explicit. Pending matched requests and completed wait stages are recorded at
  failure and successful readiness; failure marker evidence contains count/visibility and document ready state,
  not page text/HTML. These observations distinguish visible boundaries; they
  do not prove a root cause.
- Initial navigation/readiness failure retains `navigation_error`, the original
  failed stage and normalized error class, and the existing failed coverage/exit.
  Opt-in failure reasons exclude raw browser error prose. The document HTTP
  status is retained if navigation returned a response. Readiness timeouts and
  application actions are not changed, retried or warmed up.
- Before context cleanup, the verifier attempts initial-viewport then full-page
  capture from that same page, using ordinary control redaction and caller
  `screenshotMasks`. A three-second deadline covers diagnostic DOM/capture work;
  if necessary the page is closed to interrupt pending browser operations.
  Capture and close scheduling may finish just after the deadline. No recovery
  navigation or verifier scrolling occurs. Snapshot/capture timestamps identify
  the observed times; pixels are not claimed to be frozen at the timeout instant.
  Masking failure, a closed browser or capture timeout is explicitly unavailable,
  never an unmasked fallback. A partial image pair is valid diagnostic evidence,
  not complete visual acceptance. Other sensitive visible regions still require
  caller masks; arbitrary secret detection is not promised.
- Failure images use the existing hashed screenshot descriptors in the private
  report and journey-evidence bundle. The failed cell does not become `checked`
  or enter the manual acceptance queue. No Console/API schema or public access
  change is involved. Independent safe cells and normal context cleanup continue.
- Successful opted cells retain the same bounded passive event timeline, pending
  resource counts, unmatched resource-type counts and completed wait stages with
  `status: "ready"`. This adds no DOM probes, diagnostic captures or waits and does
  not infer a failure or cause from a slow successful load. They retain normal
  verification. Listeners are removed before later interactions. Opted cells
  neither read nor write development-cache evidence; subsequent invocations load
  a real fresh page. Absent/disabled diagnostics preserve the existing path.
  Authentication-profile setup, later interaction failures and historical pages
  are not captured by this option.

## Review Inputs And Evidence

- `repoRoot` is explicit and canonical. Each `reviewInputs` entry is a
  repository-relative regular file or directory. Missing, empty, external, or
  symlinked inputs fail coverage.
- Inputs identify only UI code, styles, tokens, fonts, assets, and genuinely
  shared presentation files that can change that target/state. Do not include
  the entire repository merely to avoid mapping ownership.
- Every checked cell produces a redacted initial-viewport PNG and full-page
  PNG. Native control values, placeholders, selected labels, and declarative
  fill payloads are removed or masked. Add `screenshotMasks` with a reason for
  other sensitive regions.
- Capture preserves the tested page's CSP. Temporary constructed stylesheets
  hide control text in documents, open shadow roots, and reachable frames;
  failed redaction or changed capture coordinates prevents image retention.
  Full-page capture starts at the document origin so mobile scrolling cannot
  displace masks. The original scroll position and existing styles are restored
  after successful or failed capture. The initial-viewport image still records
  the user's actual interaction position, not the full-page capture origin.
- Every run also produces `journey-evidence.json`: an ordered, path-free
  Console index of the declared route/state/viewport cells, action kinds and
  outcomes, automatic finding kinds, and the two screenshot integrity records.
  Its coverage block retains each required-cell status and matching cell IDs,
  including missing or ambiguous declarations that have no screenshot.
  It omits selectors and every action value. A governed check stores the bundle
  under its private run leaf so log retention removes the manifest and images
  together. Explicit caller-owned report paths remain unchanged; the verifier
  also atomically publishes a unique immutable copy below the leaf's
  `formal-runs/` directory, allowing separate or concurrent batches to remain
  reviewable without overwriting one another.
- Screenshot SHA-256 values bind evidence integrity only. Pixel changes never
  enter the manual-review queue.

## Ordered review receipts

Formal verification is the first gate. Its receipt has this required shape:

```json
{
  "formal": {
    "result": "passed",
    "runId": "formal-web-ui-...",
    "candidateId": "...",
    "exitCode": 0,
    "freshComplete": true,
    "sourceSha256": "...",
    "configSha256": "...",
    "verifierSha256": "...",
    "coverage": {"status": "passed", "readinessEligible": true},
    "evidence": {
      "report": "report.json",
      "journeyEvidence": "journey-evidence.json",
      "reviewQueue": "review-queue.json",
      "screenshots": "screenshots/",
      "manifestSha256": "..."
    }
  }
}
```

`formal.result` is exactly `passed`, `failed`, `blocked`, or `incomplete`.
Only a fresh complete all-cell run with exit `0`, readiness-eligible coverage,
all required cells and assertions satisfied, no blocking findings, and retained
evidence is `passed`. A blocking finding is `failed`; unavailable authentication,
rendering, screenshots, tooling, or Coordinator evidence is `blocked`; a subset,
cache hit, skipped cell, missing shape, or partial matrix is `incomplete`.
Preserve non-passing artifacts as diagnostic evidence and do not replace them
with prose. Repair the product and rerun formal verification on a fresh
candidate before starting any downstream gate.

The source digest explicitly covers declared UI-input fingerprints and matched
observed source bindings; it is not a complete-repository digest. Verifier
identity covers the canonical entrypoint and its handoff-contract module.
Candidate identity combines the actual source/config/verifier/plan identities.
`formal-artifacts.json` hashes the retained report, Markdown, queue, journey and
screenshots; `formal-receipt.json` binds that manifest without circularly hashing
itself. Legacy status/exit fields remain diagnostics; only `formal.result`
determines this gate.

### Manual screenshot review

Run this stage only when `formal.result == passed`. Read only
`review-queue.json`, open the initial-viewport and full-page pair for every
queued cell, carry unchanged cells by their prior hash-bound screenshot
identity and decision without reopening them, and finalize an independent
manual receipt:

```json
{
  "manual": {
    "result": "passed",
    "formalRunId": "formal-web-ui-...",
    "formalManifestSha256": "...",
    "reviewer": "...",
    "reviewedAt": "2026-10-02T00:00:00Z",
    "cells": [
      {
        "cellId": "registry/base/light/desktop",
        "target": "registry",
        "state": "base",
        "theme": "light",
        "viewport": {"name": "desktop", "width": 1440, "height": 900},
        "screenshots": {
          "initialViewportSha256": "...",
          "fullPageSha256": "..."
        },
        "decision": "pass",
        "note": ""
      }
    ]
  }
}
```

The receipt must enumerate every formal target/state/theme/viewport cell,
including carried cells, and bind each screenshot identity to the formal run.
`manual.result == passed` requires every cell to have a reviewer decision of
`pass`; `gap`, `blocked`, missing cells, missing screenshots, or missing notes
where required keep it non-passing. A formal failure means no screenshot is
opened and no manual pass receipt is created.

The finalizer rejects absent or non-passed formal evidence before accepting
review, and checks the retained run/source/artifact/screenshot identities. Its
separate `manual` object is retained with the compatibility decision list.
`manual.result` is `passed` only for complete passing decisions; a noted gap is
`incomplete` and an explicit blocked decision is `blocked`. Every cell records
the observed local tool caller UID and RFC3339 review time. Carried cells keep
the actual prior manifest, source/intent and screenshot bindings without
reopening unchanged images. A stale or missing upstream receipt is never a
legacy-compatible success.

The existing finalizer remains the required command:

```bash
devcoordinator2-tooling formal-ui review \
  --report /path/report.json \
  --queue /path/review-queue.json \
  --decisions /path/decisions.json \
  --out /path/manual-review.json
```

Use `gap` or `blocked` with a concrete note when appropriate. The finalizer
returns `1` while any decision blocks delivery and `2` for an invalid or
tampered evidence chain.

### Product Design and deployment receipts

When an approved visual target exists, run `$product-design:audit` only after
`manual.result == passed`. Its receipt must include the numbered journey steps,
fresh screenshot identities, source and implementation identities, UX and
accessibility findings, evidence limits, P0–P3 classification, iteration
history, and the exact text `final result: passed`. Design QA alone is not this
receipt. Any P0–P2 finding or missing evidence blocks handoff.

After the applicable Product Design receipt passes, bind live verification to
the same candidate with this minimum deployment receipt:

```json
{
  "deployment": {
    "result": "passed",
    "formalRunId": "formal-web-ui-...",
    "manualReceiptId": "manual-...",
    "productDesignReceiptId": "audit-...",
    "sourceSha256": "...",
    "artifactManifestSha256": "...",
    "imageDigest": "...",
    "deploymentGeneration": 3,
    "liveRoute": "https://preview.example.test/registry",
    "renderedEvidence": ["screenshot-sha256:..."],
    "httpStatus": 200,
    "contentType": "text/html"
  }
}
```

The Product Design receipt ID is omitted only when no approved target exists,
with that applicability decision retained. HTTP 200, container health,
matching static assets, or a successful deployment command cannot establish
this gate alone. Finish with the Coordinator's qualified
`release.deliver_evidence` receipt; a deployment metadata row is not delivery
proof.

### Changed visual review compatibility

The existing changed-review finalizer still accepts the compact input shape:

```json
{
  "decisions": [
    {
      "reviewCellKey": "key from review-queue.json",
      "decision": "pass",
      "note": ""
    }
  ]
}
```

```bash
devcoordinator2-tooling formal-ui review \
  --report /path/report.json \
  --queue /path/review-queue.json \
  --decisions /path/decisions.json \
  --out /path/manual-review.json
```

Supply a prior reviewed manifest explicitly on the next run:

```bash
devcoordinator2-tooling formal-ui verify \
  --config formal-web-ui.json \
  --review-against /path/prior-manual-review.json
```

Unchanged prior passes and gaps are carried without reopening their images;
gaps remain blocking. New cells and changed review-input or intent fingerprints
enter the queue. When a previously reviewed cell is deliberately removed,
declare its key and reason under `reviewRemovedCells`; silent removal fails
coverage.
