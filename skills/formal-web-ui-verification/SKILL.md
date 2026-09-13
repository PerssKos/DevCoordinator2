---
name: formal-web-ui-verification
description: Run journey-aware browser verification of rendered web UI hierarchy, navigation TTFB/LCP, immediate interaction continuation, WCAG text contrast, declared-theme balance, palette risks, geometry, scroll topology, responsive samples, and truthful route/state/viewport evidence. Use when frontend work needs deterministic performance and layout checks plus initial/full-page screenshots and changed-input-only agent review for slow response/rendering, displaced primary content, offscreen forms, unreadable colors, contradictory themes, nested scrolling, clipping, overlap, broken media, stale deployments, or incomplete coverage.
---

# Formal Web UI Verification

## Overview

Use this skill to inspect rendered web interfaces with deterministic DOM
geometry and computed
style measurements instead of relying only on screenshots or model vision. The
bundled verifier injects JavaScript into real pages through Playwright, checks
desktop and mobile viewports, emits JSON plus Markdown evidence, and exits
nonzero when findings meet the configured severity threshold or when required
targets could not be checked. It is a heuristic browser-side detector, not a
mathematical proof of UI correctness; inaccessible states and browser contexts
remain explicit coverage limits.
Text detection covers any element that directly owns rendered text (including
`div`-based layouts), not just classic heading/paragraph tags, and clipping
checks cover both self-overflow and cuts made by ancestor `overflow` clipping
(absolute children, negative offsets, parent crops). By default it runs a
full-page scroll pass before measuring so below-the-fold and lazy-loaded
content is exercised. Every run inventories visible/active document and
element scrollbars, records their same-axis containment chains, warns for every
horizontal scrollbar and every second vertical layer, and blocks nested
horizontal or third-and-deeper vertical layers. It also records contrast it
could not measure against a solid
background, traverses discoverable open shadow roots, evaluates every
Playwright-reachable iframe, records any reachable context it could not inspect, and lists
allowed ellipsis/line-clamp truncations, hidden text-like elements, and
still-loading media, even when the page has no critical layout findings.
Native modal dialogs exclude their browser-inert background from active-content
defects. Fixed descendants count as reachable only when an actual inner scroll
range on the cut axis can expose them within the viewport. Locked scroll regions,
unscrollable fixed controls and occlusion inside the active dialog still fail.
Deliberately temporary,
positioned dialog/menu/listbox/tooltip/popover surfaces may declare
`data-ui-contextual-overlay="reason"` so their coverage of outside content is
recorded as `allowed-contextual-overlay`. This does not exempt their own
contents from occlusion, clipping, contrast or geometry checks. An empty reason,
unpositioned element or unrelated undeclared cover receives no exception.
If a real gutter-scroll probe reaches that contextual surface, record the
declared cover rather than claiming the token is still hidden by the gutter.
Undeclared covers reached after scrolling remain failures; probes restore their
original scroll offsets.
Native input placeholders and every selectable native option label are measured
against the control's rendered inner content width, including the native select
affordance, without retaining the measured text or any entered value. Visible
native inputs, textareas, and selects must also remain inside each non-scrollable
layout ancestor; an active horizontal scroll path or explicit overlap allowance
records intentional exceptions. Pages may opt important cards/forms into a
minimum readable-content inset contract. Targets may also opt into exact
breakpoint-minus-one/at/plus-one samples under a hard
page-cell budget.
Each checked final main document is also measured before verifier scrolling:
local navigation TTFB must be strictly below 10 ms and document LCP strictly
below 800 ms unless global or target-specific thresholds prescribe other
values. Threshold breaches are critical; unavailable required metrics remain
explicit warnings.

Every target and interaction state also carries an explicit user-journey
contract: one primary journey with frequency/risk context, semantic rendered
regions, light/dark/mixed theme intent, and repository-relative UI
implementation inputs. The verifier measures hierarchy in the initial
viewport before scrolling and checks that an activated in-page journey reveals
a visible focused continuation without a document jump. Bare URL targets fail
coverage because geometry without product intent cannot prove that the right
content owns the page.

The execution plan is safe-complete rather than fail-fast. Ordinary cell
failures are collected while later independent work continues; browser-authority
or evidence-safety loss is the explicit stop boundary. Optional bounded
concurrency, journey priority, in-memory authentication profiles, changed-input
development selection, and an exact caller-owned evidence cache improve speed
without weakening the fresh complete run required for readiness. Read
[references/journey_review_contract.md](references/journey_review_contract.md)
before using these advanced contracts.

Each checked cell automatically emits a redacted initial-viewport screenshot
and full-page screenshot plus a changed visual-review queue. Screenshot hashes
prove integrity only. Manual review repeats only for new cells or changed
declared UI inputs or journey/theme intent, never because dynamic pixels differ.
Read [references/journey_review_contract.md](references/journey_review_contract.md)
when preparing this contract or finalizing agent review.

This deterministic verification layer is not a replacement for human visual
judgment. Use it before reporting changed web UI as done, and include its
critical findings in the implementation or audit result.

Complex SVG internals remain excluded from ordinary overlap sampling because
charts, maps, icons, and artwork legitimately layer many elements. Important
SVG text can opt into deterministic internal collision protection with
`data-ui-verify-svg-overlap="reason"`. A protected text element must not
intersect another rendered SVG text element or painted filled shape in the same
SVG unless either side carries an explicit `data-ui-allow-overlap` allowance.

## Quick Start

Resolve the skill directory from the loaded skill path and run the self-test
before relying on the verifier in a new environment. When an agent cannot
provide the loaded path, require the caller to supply the absolute skill
directory explicitly:

```bash
FORMAL_WEB_UI_SKILL_DIR="/absolute/path/to/formal-web-ui-verification"
devcoordinator2-tooling formal-ui self-test
```

The self-test resolves Playwright from an explicit module directory derived
from the canonical repository's locked `ci/playwright/node_modules`
installation, `FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES`, or an existing
dependency environment, then passes `--playwright-module-dir` to every
verifier subprocess. It never relies on the temporary audited working
directory. In a source checkout where the locked dependency is absent, run
`npm ci --ignore-scripts --prefix ci/playwright` once; the self-test reports
this exact recovery instead of requiring callers to guess a `NODE_PATH`.

Verify explicit routes through a complete config (the linked contract reference
contains a full example):

```bash
devcoordinator2-tooling formal-ui verify \
  --config formal-web-ui.json \
  --fail-on critical
```

Artifact-first output is the software-owned default. With no output paths, the
verifier creates a unique directory under a safe external runtime root
(normally the system temporary directory) outside the current audited worktree,
writes complete `report.json`, `report.md`, `journey-evidence.json`, `review-queue.json`, bounded
`progress.jsonl`, and redacted
initial/full-page screenshot artifacts, and emits one bounded JSON receipt
naming them. Exit codes `0`/`1`/`2`/`3` are preserved. `--json-out` and
`--markdown-out` override those paths when a caller needs a known cold artifact
location; if only one is supplied, the verifier derives its companion path.
Store verbose wrapper, browser, or self-test output under the task's cold log
directory and inspect only the receipt and targeted report sections in routine
context.

Inside a governed DevCoordinator2 check, the executor supplies one private
run/leaf evidence directory. With no explicit output override, the verifier
writes its complete bundle there, including a strict path-free
`journey-evidence.json` manifest. When a caller deliberately keeps reports and
screenshots at explicit artifact paths, the verifier preserves those outputs
and atomically publishes a confined immutable Console copy under
`formal-runs/<bundle>/` in the same evidence leaf. Separate or concurrent
formal batches receive distinct bundle directories, so no valid journey is
replaced. Each manifest orders every declared route/state/viewport cell, binds
its masked screenshots by SHA-256 and actual dimensions, and omits action
values and selectors. The Console may replay every bundle while the owning run
remains within test-log retention; the original PNG files never become editable
review state.

Full Markdown stdout is compatibility behavior for an attended human terminal
only. It requires the explicit `--human-readable-stdout` opt-in and still writes
both artifacts. Never use that human-only flag for an agent-driven invocation.
Setup/configuration failures also emit a bounded receipt and write a
machine-readable failure artifact whenever a safe artifact destination can be
created.

Previously published callers may still pass `--receipt-only` or boolean config
`receiptOnly`. Both are accepted as deprecated compatibility no-ops: receipt
output remains the default, and `receiptOnly: false` never enables full stdout.

Auth-gated pages and local TLS: pass `--cookie name=value` (repeatable; scoped
to the target URL) to verify pages behind a session cookie, and
`--ignore-https-errors` when the target uses a self-signed or otherwise
untrusted certificate (local HTTPS dev servers). Both are also accepted in the
JSON config as `cookies` (strings or `{name, value, url?, domain?, path?}`)
and `ignoreHttpsErrors`.
For repeated role setup, declare an `authProfiles` bootstrap and bind targets by
name. The resulting Playwright storage state exists only in memory for that run;
it is never written to reports, logs, source, or cache entries.

Exit codes are stable: `0` checked with no blocking findings, `1` blocking UI
findings, `2` setup/configuration failure, and `3` required-target coverage
failure. Explicit targets fail closed on navigation, HTTP, or non-HTML errors.

Verify healthy coordinator-managed web URLs without starting duplicate servers:

```bash
devcoordinator2-tooling formal-ui verify \
  --config formal-web-ui.json \
  --coordinator-command devcoordinator2 \
  --only-current \
  --fail-on critical
```

Coordinator discovery is an optional adapter to a separately installed skill.
The verifier has no source, checkout, build, CI, or version dependency on that
skill; callers can always provide explicit `--url` targets instead.

## Workflow

1. **Find a safe render path**
   - Prefer a test, fixture, Storybook, preview, or coordinator-managed local
     dev URL.
   - Before starting, stopping, or replacing a dev server, use
     `dev-coordinator`.
   - Do not use production or side-effecting flows unless the user explicitly
     asked for them and the route is safe to inspect.

2. **Run the formal verifier**
   - Read the product/journey sources first. Build the target contract described
     in `references/journey_review_contract.md`; do not infer priority from DOM
     order or feature names. Every effective target/state must declare journeys,
     primary journey, regions, theme, and review inputs.
   - Check at least one narrow/mobile viewport and one desktop viewport for web
     UI changes.
   - When the journey depends on touch, mobile user-agent behavior, device pixel
     ratio, or mobile browser layout semantics, use a Playwright descriptor such
     as `{"name":"iphone","device":"iPhone 13"}`. A narrow desktop viewport is
     useful responsive evidence, but it is not equivalent to device emulation.
   - Declare transient states under a target's `states` list. Each state runs in
     a fresh browser context after an ordered set of bounded `click`, `hover`,
     `focus`, `fill`, `check`, `uncheck`, `press`, or `selectOption` actions.
     Action failures fail the target-coverage gate; arbitrary injected JavaScript
     is deliberately unsupported. Values used by actions are omitted from the
     public report.
   - Declare mandatory target/state/viewport cells in top-level
     `requiredCoverage`; add `width` when an exact reported or CSS-boundary
     width matters. A missing or ambiguous required cell fails coverage with
     exit `3`. The verifier never infers that a hidden menu, dialog, or
     expander was tested from a closed base page.
   - A conditional action may name `ownerJourney` and `ownerState`. Outside that
     owner, an absent or hidden control hands off immediately to the exact
     planned owner cell. Visible contradictions, missing/duplicate owners, and
     failed owner cells fail coverage.
   - Readiness waits use observable events: response, URL/navigation, selector
     or ready/error selector race, server readback, load state, or one/two render
     frames. Longer timeouts are failure ceilings. Deliberate delays and polling
     intervals above 100 ms are rejected.
   - Rendered-navigation performance is measured from the browser's Navigation
     Timing entry and a buffered LCP observer installed before navigation. The
     defaults are `TTFB < 10 ms` for localhost/loopback targets and `LCP < 800
     ms` for every final main document. Configure global `performance` values
     or a target/state override when the product prescribes different limits;
     set `ttfbLocalOnly: false` to apply its TTFB threshold to non-local targets.
     LCP here is document navigation LCP, not post-interaction latency.
   - A target may declare `breakpointProfile` with `breakpoints`, `height`, and
     optional `name`/`baseViewport`. Each breakpoint adds exactly width−1,
     width, and width+1 for that target. Equivalent configured/profile cells
     are de-duplicated. The complete target × state × viewport expansion must
     fit `maxPageCount` (default `60`); exceeding it is a setup failure, never a
     silently reduced sample.
   - Treat every viewport result as sampled-only evidence. The JSON and
     Markdown reports list the exact widths checked and explicitly state that
     widths between samples were not inspected.
   - `execution.maxConcurrency` bounds the worker pool. Only targets/states with
     explicit `parallelSafe: true` may overlap; shared `resourceLocks` serialize
     conflicts and undeclared work remains exclusive. Risk, frequency, and an
     optional explicit priority affect start order only, never coverage or final
     report order.
   - `development.changedPaths` selects mapped target groups for a fast feedback
     run. Any unmapped path expands safely to the full plan. Every development
     run is marked ineligible for readiness even when it executes all cells.
   - An optional development cache requires an explicit existing external
     directory, `dataRevision`, source binding, and declared review inputs. It
     stores only privacy-safe page evidence plus masked screenshot pairs under
     an exact content key. It never stores auth state, never discovers a hidden
     latest entry, and never replaces a fresh complete readiness run.
   - The verifier scrolls the full page top-to-bottom before measuring so
     below-the-fold and lazy-loaded content is exercised; how far it scrolled is
     reported in `metrics.scroll`. Pass `--no-scroll` (or `"scroll": false` in a
     config) only when the page must not scroll during inspection.
   - Use `--from-coordinator --only-current` only for already-running current
     coordinator URLs. A discovered URL that cannot be checked fails the
     coverage gate by default. Use `--allow-discovered-target-failures` only
     when stale discovery is expected and the incomplete coverage is acceptable.
   - An optional explicit target may use `"allowFailure": "reason"` in config.
     The exemption remains in the report. `minCheckedPages` defaults to `1`, so
     a run cannot pass solely through exemptions unless the config deliberately
     sets the minimum to `0`.
   - When source currency matters, declare `sourceBinding.expected`. The
     verifier compares it with the final deployment's
     `X-UI-Source-Revision` response header by default, falling back to
     `<meta name="ui-source-revision">`; `responseHeader` and `metaName` are
     configurable or nullable. A missing or mismatched observed value fails
     coverage. A final origin/path different from the requested origin/path
     (including a redirect to sign-in) also fails coverage and is not counted
     as checked.
   - Keep `--fail-on critical` as the default for low-noise delivery gates.
     Use stricter settings only when the project asks for warning-level gates.

3. **Interpret findings**
   - Treat `critical` as blockers before delivery.
   - Treat `warning` as review evidence: fix when relevant to the journey, or
     document why it is acceptable.
   - `unmeasurable-contrast` and `not-inspected` are always warnings, never
     criticals: they mark coverage gaps (gradient/image or translucent
     backgrounds; a reachable iframe whose execution context could not be
     evaluated). Open shadow roots and Playwright-reachable frames are inspected;
     closed shadow roots cannot be discovered from outside the component.
     Review uncovered contexts visually rather than trusting a pass.
   - `clipped-hidden`, `offcanvas-hidden`, and `fixed-offscreen-hidden` mark
     content that is fully invisible in a way that is often intentional
     (closed accordions, offscreen slides/drawers, skip links). When the
     hidden element belongs to the journey under verification, confirm the
     state is intentional instead of treating the warning as noise.
   - Do not suppress a finding globally just to pass. Per-target ignores or
     allowances must name a selector and a reason.

4. **Report evidence**
   - Keep the complete JSON, Markdown, journey-evidence manifest, review queue, and screenshot pairs at the
     receipt-named artifact paths. Do not paste full artifacts into chat or a
     parent-agent result.
     Return only the outcome, exit code, checked/skipped page counts, critical
     and warning counts, coverage status, and artifact paths.
   - Keep `progress.jsonl` as bounded cold evidence while the safe-complete pass
     runs. It records plan/execution indices, outcome, timing, cache, and cleanup
     only; it never streams action, cookie, auth, or control values.
   - The complete reports bind evidence to run/per-cell start and end times,
     verifier SHA-256, a privacy-safe normalized effective-config SHA-256,
     requested/final paths, every exact route/state/viewport cell, sampled-only
     widths, and deployment/source binding status. Cookie values,
     authentication/readback expectations, declarative action values, typed
     values, placeholders, and selected labels are never
     included in the config hash input or control findings.
   - Performance evidence retains only the metric value, strict threshold,
     pass/fail/unavailable state, navigation type, LCP size, local-target flag,
     and timing-entry source. It never retains the LCP element, its text, or a
     resource URL.
   - Read only the specific finding rows or metrics needed for diagnosis; keep
     raw command output in a cold log file.
   - If no safe render path exists, report that formal verification is blocked
     and add the missing Playwright, Storybook, fixture route, or preview path
     to the implementation plan.

5. **Review changed visual evidence after automation**
   - Finish the formal verifier and every other applicable automatic test first.
   - Read `review-queue.json`, open only each queued cell's initial-viewport and
     full-page screenshots, and record `pass`, `gap`, or `blocked`. Never reopen
     carried unchanged screenshots; carried gaps remain blocking.
   - Finalize and validate `manual-review.json` with
     `devcoordinator2-tooling formal-ui review` as shown in the contract
     reference. A
     formal exit `0` with pending review is not visual completion.

## Default Rule Set

Critical findings by default:

- Missing or displaced primary-journey content in the initial viewport, a
  lower-priority workflow before it, or oversized supporting content before it
  (`primary-journey-content-missing`,
  `primary-journey-outside-initial-viewport`,
  `secondary-workflow-precedes-primary`,
  `supporting-content-dominates-primary`). Missing target intent is a coverage
  failure rather than a finding.
- Activated in-page work whose continuation is missing, offscreen, unfocused,
  or reached through an unexpected document jump
  (`continuation-anchor-missing`, `continuation-anchor-offscreen`,
  `continuation-anchor-not-recognizable`, `continuation-focus-missing`,
  `continuation-document-jump`).
- Document horizontal overflow (`document-horizontal-overflow`).
- A horizontal scrollbar nested inside another active horizontal scroll path
  (`nested-horizontal-scrollbars`).
- A third or deeper active vertical scroll layer
  (`triple-nested-vertical-scrollbars`).
- Text/controls clipped by their own `overflow: hidden`/`clip`
  (`clipped-x`/`clipped-y`) without a scroll path or explicit allowance.
- Rendered native input placeholders or any selectable option label wider than
  the control's real inner content width (`control-text-clipped`). A short
  selected label does not excuse a longer unselected option. Evidence contains
  only control kind, option count, and geometry; no control text or entered
  value is retained.
- A visible native input, textarea, or select escaping a non-scrollable layout
  ancestor (`control-outside-container`). An active horizontal scroll path is
  reachable; a reasoned `allowOverlap` exception produces `allowed-overlap`.
- Rendered text/control content closer to a container edge than an explicitly
  declared minimum readable inset (`content-inset-below-minimum`). There is no
  universal spacing heuristic.
- Text/controls partially cut by an ancestor's `overflow: hidden`/`clip`
  (`clipped-by-ancestor`): absolute children sticking out of cropped
  containers, negative-margin cuts, nowrap text spilling into a clipping
  parent. Scrollable ancestors on the cut axis count as a reachability path and
  do not fail. Inside carousel/slider-marked containers the finding is
  downgraded to a warning; fully hidden content becomes a `clipped-hidden`
  warning because closed accordions/tabs/slides are often intentional.
- Unrelated overlap/occlusion of meaningful text or controls: fully covered
  (`occluded`) or covered on ≥60% of sampled points (`partially-occluded`).
  Every candidate starts from the verification state's original document and
  inner-scroll positions, including open shadow roots. Probes restore those
  exact coordinates after each candidate and on failure, without smooth-scroll
  animation, so they cannot move a later measurement or the final page.
  Elements already in the viewport are checked at their natural on-screen
  position; only off-screen elements are scrolled into view first and their
  findings are tagged `measuredAfterScroll`. A near-transparent occluder is
  downgraded to a warning.
  Text behind a horizontally edge-pinned sticky sibling is reachable only when
  scrolling their shared container actually exposes the same sampled text
  points. This is not a selector allowance: oversized gutters, exhausted scroll
  ranges, non-scrollable clipping and unrelated overlays remain checked.
  Focused rendered regression proof lives in
  `rust/tooling/tests/formal_occlusion.mjs` and runs in the Rust state self-test.
- An active focused custom modal (`role=dialog|alertdialog`, `aria-modal=true`)
  excludes only outside subtrees that are both `inert` and `aria-hidden=true`.
  Modal contents remain fully checked; an unfocused or mislabeled modal and
  `aria-hidden` alone never suppress genuine occlusion. Native modal behavior
  remains unchanged, including dialogs in shadow roots.
- A `text[data-ui-verify-svg-overlap]` element intersecting another rendered
  SVG text element or painted filled shape in the same SVG
  (`svg-internal-overlap`). This opt-in rule is critical, emits bounded
  rectangle and selector evidence without retaining the protected text, and
  honors `data-ui-verify-ignore` and `data-ui-allow-overlap` on either side.
- Text/controls partially cut by an unreachable edge: before the document
  origin (`offcanvas-cut`), fixed-position content cut by the viewport
  (`fixed-offscreen-cut`), or interactive controls beyond the horizontal
  document scroll range (`interactive-offscreen-x`).
  Negative coordinates caused by an existing inner scroll position are not
  unreachable when the visible scroll container can expose that content.
  This applies to fixed and non-fixed layouts; content outside the actual
  scroll range still fails, and measurements preserve the original offsets.
- Controls or text outside a configured area of interest (`outside-area`).
- Broken images/videos (`broken-image`/`broken-video`), including broken
  images that collapsed to ~0x0 because their source failed.
- Text below WCAG 2.2 AA against a measurable solid background: 4.5:1 for
  normal text and 3:1 for large text (`insufficient-text-contrast`), plus text
  with effectively invisible foreground/background contrast against a
  genuine solid background (`invisible-text`). When the effective background is
  a gradient, image, or unresolved translucent stack, contrast is not computed
  against white; the element is recorded in `metrics.unmeasurableContrast`
  and reported only as a warning. Solid translucent layers over a known opaque
  background use source-over compositing with accumulated alpha; readable
  dark/light surfaces and genuinely insufficient contrast have rendered
  regression fixtures (`formal-ui self-test --phase rendering`).
- A large opposite-brightness surface that contradicts a declared light or dark
  target (`declared-theme-contradiction`). Mixed themes and selector-specific
  reasoned exceptions retain measurements without this failure.
- Browser-measured navigation TTFB at or above its applicable threshold
  (`ttfb-above-threshold`) or document LCP at or above its threshold
  (`lcp-above-threshold`). Defaults are strict `< 10 ms` local TTFB and `< 800
  ms` LCP; equality fails.

Warnings by default:

- Every visible/active horizontal scrollbar raises `horizontal-scrollbar`, even
  at same-axis depth one, because horizontal scrolling is exceptional. A
  second horizontal layer is also the critical
  `nested-horizontal-scrollbars` finding. A second vertical layer raises
  `double-nested-vertical-scrollbars`; the third and every deeper vertical
  layer is critical. The document scrollbar counts as a layer, differently
  directed ancestors do not increase same-axis depth, and an explicit
  `overflow-*: scroll` bar counts even when it has no current scroll range.
  `metrics.visibleScrollbars` and the Markdown `Visible Scrollbars` section
  retain each outer-to-inner chain as evidence. A lone vertical scrollbar
  remains inventory-only.
- Coverage gaps are always reported in the Markdown `Coverage & Unmeasurable`
  section: `metrics.unmeasurableContrast`, `metrics.notInspected`
  (discovered/inspected open-shadow and iframe counts plus reachable frame
  evaluation failures, which raise a `not-inspected` warning),
  `metrics.ellipsisTruncations` (allowed single-line ellipsis and
  line-clamp truncations), `metrics.hiddenTextLike` (text/controls present in
  the DOM but not rendered), and `metrics.pendingMedia` (media still loading at
  measurement time).
- A required TTFB or LCP value that the browser could not expose
  (`performance-metric-unavailable`). Non-local TTFB under the default
  local-only scope is explicitly `not-applicable`, not unavailable or passed.
- Fully hidden clipped content (`clipped-hidden`), fully offscreen
  fixed/static content (`fixed-offscreen-hidden`/`offcanvas-hidden` — the
  skip-link/visually-hidden pattern), and carousel-context cuts.
- Partial overlap below the 60% threshold (`partially-occluded` with ≥2 covered
  sample points).
- Small interactive targets (`tiny-interactive-target`).
- Explicitly allowed truncation (`allowed-truncation`).
- Explicitly allowed WCAG contrast exceptions (`allowed-contrast`).
- Less-severe declared-theme imbalance, high-chroma surface dominance, four or
  more prominent accent-hue clusters, or theme surfaces made unmeasurable by
  gradients/media (`declared-theme-balance-risk`,
  `high-chroma-surface-risk`, `competing-accent-hues`,
  `unmeasurable-theme-surface`). These are evidence for agent judgment, not
  automatic claims that a palette is unattractive.
- Broad container overflow that belongs to charts, maps, canvases, or other
  complex artifacts (`complex-artifact-overflow`). Artifact detection is
  token-bounded: an ancestor must be a real `svg`/`canvas`, match a known
  map/chart library token (leaflet, mapbox, recharts, echarts, plotly, …), or
  carry a generic map/chart token while actually containing a substantial
  svg/canvas/video. Sections merely named `roadmap`, `sitemap`, or similar are
  NOT excluded from checks.
- Findings are capped at 40 per rule per page; a `findings-truncated` warning
  with per-rule suppressed counts is emitted when the cap is hit, so mass
  breakage cannot silently vanish from the report.

## Areas, Insets, Ignores, And Allowances

Prefer attributes in source or fixture markup when the policy should travel
with the component:

```html
<section data-ui-verify-area="editor-preview">
  <button>Save</button>
</section>

<section class="items-card" data-ui-verify-min-content-inset="12">
  <span>Items</span>
</section>

<span data-ui-allow-truncation="filename may ellipsize">very-long-file-name.pdf</span>
<span data-ui-allow-contrast="inactive watermark">Draft</span>
<figure data-ui-theme-exception="intentional document preview">...</figure>
<div data-ui-allow-overlap="intentional floating toolbar">...</div>
<div data-ui-verify-ignore="third-party map internals">...</div>
<svg>
  <text data-ui-verify-svg-overlap="chart heading must stay outside the plot">Revenue</text>
</svg>
```

Use a config file when allowances are route-specific:

```json
{
  "repoRoot": "/absolute/path/to/repository",
  "targetDefaults": {
    "journeys": [{"id": "review-dashboard", "frequencyPercent": 100, "risk": "normal"}],
    "primaryJourney": "review-dashboard",
    "regions": [{"selector": "[data-ui-region='dashboard-primary']", "role": "primary-content", "journey": "review-dashboard"}],
    "theme": "light",
    "reviewInputs": [
      {"path": "src/dashboard", "kind": "ui-code"},
      {"path": "src/styles/dashboard.css", "kind": "style"}
    ]
  },
  "targets": [{
    "name": "dashboard",
    "url": "http://127.0.0.1:3000/dashboard",
    "breakpointProfile": {
      "name": "dashboard-layout",
      "breakpoints": [768, 1024],
      "height": 900,
      "baseViewport": "desktop"
    },
    "contentInsets": [{"selector": ".important-card", "min": 12}],
    "sourceBinding": {
      "expected": "git:abc123",
      "responseHeader": "x-ui-source-revision",
      "metaName": "ui-source-revision"
    },
    "states": [{
      "name": "account-menu-open",
      "actions": [{"action": "click", "selector": "[aria-label='Account']"}],
      "waitFor": {"selector": "[role='menu']"},
      "continuation": {"kind": "in-page", "anchor": "[role='menu']", "focusWithin": "[role='menu']"}
    }]
  }],
  "viewports": [
    {"name": "iphone", "device": "iPhone 13"},
    {"name": "desktop", "width": 1440, "height": 900}
  ],
  "requiredCoverage": [{"target": "dashboard", "state": "account-menu-open", "viewport": "desktop", "width": 1440}],
  "areas": [{"name": "main", "selector": "main"}],
  "ignore": [{"selector": ".third-party-map", "reason": "vendor map internals"}],
  "allowTruncation": [{"selector": ".filename", "reason": "intentional ellipsis"}],
  "allowOverlap": [{"selector": ".floating-toolbar", "reason": "intentional overlay"}],
  "allowContrast": [{"selector": ".inactive-watermark", "reason": "incidental decorative text"}],
  "themeExceptions": [{"selector": ".document-preview", "reason": "preview preserves source colors"}],
  "screenshotMasks": [{"selector": ".customer-email", "reason": "sensitive fixture data"}],
  "maxPageCount": 18,
  "performance": {"ttfbMs": 10, "lcpMs": 800, "ttfbLocalOnly": true},
  "scroll": true,
  "rules": {"failOn": "critical", "strictTruncation": false}
}
```

Set `"scroll": false` (or pass `--no-scroll`) to disable the full-page scroll
pass when a page must not scroll during inspection.

`contentInsets` may be global or target-specific and is deliberately opt-in.
The verifier measures rendered direct-text ranges and descendant control boxes
against each declared container's inner border box. Do not declare the
contract on fieldsets or attached-control groups whose content is intentionally
edge-aligned. `data-ui-allow-truncation` and `allowTruncation` also apply to
native control text; an overflowing declared exception becomes an
`allowed-truncation` warning with redacted text. `data-ui-allow-overlap` and
`allowOverlap` may document intentional native-control container escape; the
exception remains an `allowed-overlap` warning.

## Completion Rules

- Do not report changed web UI as verified if the formal verifier found
  unresolved critical findings on the relevant desktop or mobile route.
- Do not claim formal journey coverage for a bare URL or a target/state missing
  journeys, primary region, theme, review inputs, or an applicable continuation
  checkpoint.
- Do not report a run as verified when it exits `3`, checks fewer than the
  configured minimum pages, omits or ambiguously maps a required
  target/state/viewport/width cell, follows an unexpected final route, or has a
  missing/mismatched declared deployment/source binding.
- Do not describe responsive width coverage as exhaustive. Report the exact
  sampled widths and retain the hard `maxPageCount` plan evidence.
- Do not report a checked page as verified while it has an unresolved TTFB or
  LCP threshold critical. Do not treat an unavailable required metric as a pass
  or describe document LCP as interaction responsiveness.
- Do not use a changed-input subset or any cache hit as readiness evidence.
  Readiness and visual completion require a fresh complete all-cell run.
- Do not serialize an authentication storage snapshot. Reuse it only in memory
  to seed fresh contexts, and do not infer server-side data isolation from
  browser-context isolation.
- Do not treat screenshots alone as formal evidence for clipped text, overlap,
  off-canvas controls, or invisible text when this verifier can run.
- Do not claim protected SVG labels are collision-free while an applicable
  `svg-internal-overlap` finding remains unresolved. Unmarked SVG internals
  remain outside this deterministic contract and require product-specific
  assertions or visual review.
- Do not claim visual completion while `review.pendingCount` is nonzero, a
  current/manual or carried review decision is `gap`/`blocked`, or the supplied
  prior manifest/removed-cell disposition fails validation. Review only queued
  images after automatic tests; screenshot pixel drift never justifies opening
  an unchanged cell.
- Keep generated reports outside the product repo unless the user asks to save
  them there. The executor-supplied private governed-run evidence directory is
  the reviewed exception; it is retained and pruned with that exact run leaf.
- For agent-driven runs, keep the default bounded receipt output. A full
  Markdown stdout payload is a context-budget failure even when the report is
  otherwise valid; `--human-readable-stdout` is human-only compatibility.
