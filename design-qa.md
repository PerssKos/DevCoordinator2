# Final localization and capture-review handoff — 2026-09-26

final result: passed

## Selected target and current implementation

- Approved source target: retained Coordinator sketch `sf402bd6a8359364d`, set `Sketch set selection UI directions`, titled `Option 3 - Focused comparison drawer`, decision `keep` revision 3. The retained source record reports SHA-256 `22f454e5f26d3294a7594d6c3835c49df385fa4317e2825a22818bc06d8eae3d` and source dimensions 1028 × 1529.
- Source image opened for this comparison: `design-qa/sketch-annotation/approved-option-3.png` (1263 × 1246 pixels).
- Fresh implementation capture: `/var/tmp/dc2-localization/fresh-sketch-implementation-dark-1263x1246.png` (1263 × 1246 pixels, CSS viewport 1263 × 1246, device scale 1).
- Paired comparison: `/var/tmp/dc2-localization/fresh-sketch-source-vs-implementation.png`.
- Implementation route: `#/sketches/r250e950109d97973?set=Sketch%20set%20selection%20UI%20directions&sketch=sf402bd6a8359364d`, dark theme, real Coordinator sketch records and repository data.

The implementation keeps the selected focused comparison structure: a persistent Console shell, the set-review heading and option count, a central evidence canvas with annotation tools, a right decision/details and comment panel, and the option strip below. The source uses illustrative A4 content; the implementation shows the current retained sketch content and real repository names, which is the required truthful data deviation.

## Step-linked audit

1. **Open the selected set and option** — pass. The route loads the retained set, selected option, decision state, and image without a loading or error surface.
2. **Review the comparison canvas** — pass. The central image, viewport/full-page controls, annotation toolbar, and option navigation remain visible and usable.
3. **Inspect decision and comment context** — pass. Keep/Reject/Undecided, generation record, capture details, comment target, and Post comment controls are present in the review context.
4. **Change and preserve review state** — pass. The existing rendered journey covers selection, annotation creation, save failure and retry, reload persistence, deletion, focus, and narrow layouts. The repaired evidence journey passes 185/185 checks in both themes at 1280, 1239, 927, and 390px.

## Fidelity and findings

- **Typography:** Console type hierarchy, compact operational labels, native-language strings, and small evidence metadata remain readable; no P0–P2 issue.
- **Spacing and layout:** The shell, evidence board, right inspector, and option strip preserve the selected hierarchy. The repaired responsive breakpoint keeps the narrow bottom-sheet control visible and prevents the closed inspector from occupying the desktop grid.
- **Colors and tokens:** Existing dark/light Console tokens, focus rings, decision colors, and annotation colors are reused; no contrast or theme contradiction was found in the checked journeys.
- **Image and asset fidelity:** Retained sketch images and existing Tabler controls are used; no placeholder or hand-drawn replacement appears.
- **Copy and content:** Decision labels, capture facts, comments, and locale strings describe real state. Illustrative source text is not copied into live records.

The first integrated comparison had one P1 usability issue: at narrow widths the full Console verifier clicked the desktop Details control while the open inspector covered it. The fix routes narrow close actions through the visible mobile inspector toggle and aligns the JavaScript content breakpoint with the CSS viewport breakpoint. A second verifier correction scopes retained-image assertions away from the language selector's real flag images. Fresh focused evidence after these fixes has no actionable P0, P1, or P2 finding.

## Verification evidence

- Enabled catalog validation: 47 locales, 1,293 source messages, zero errors.
- Focused localization/runtime/workspace/static checks: 15 tests passed.
- Focused evidence layout: 185 checks passed, zero failures.
- Fresh paired source/implementation comparison inspected at matching 1263 × 1246 pixels.
- Full Console candidate run and final Rust workspace validation are required before release publication; their receipts are recorded with the final handoff.

## Evidence limits and accepted deviations

The source is an ideation image rather than a pixel-level specification for live data, repository chrome, or every responsive state. The current implementation uses real Coordinator records and the established Console shell; those differences are intentional and preserve the selected product decision. Screenshots do not establish complete accessibility compliance; keyboard, persistence, error, retry, deletion, and responsive behavior are covered by rendered journeys.

## Final checklist

- [x] Selected source target and decision resolved from Coordinator records.
- [x] Fresh implementation capture and paired comparison inspected.
- [x] Typography, spacing, tokens, assets, copy, responsive behavior, and interaction states reviewed.
- [x] No actionable P0, P1, or P2 finding remains.
- [x] Rendered interaction verification covers the repaired surface.
- [x] `final result: passed`.

# Historical language selector audit — superseded

final result: historical

## Source and implementation

- Source visual truth: Coordinator-retained sketch `s3bb42e5e6bb75208` in batch `kb6fd7388b447ab70` (the source image was captured from the selected Product Design result); selected by the owner under `DC2-LOCALIZATION-MENU-20260923`.
- Fresh implementation capture: `/var/tmp/dc2-localization/audit-exact-final/translated-uk-dark-1487.png`.
- Paired comparison: `/var/tmp/dc2-localization/audit/option1-vs-implementation.png`.
- Both are 1487 × 1058 pixels at CSS viewport 1487 × 1058 and device scale 1; no density normalization was needed.
- State: Plan route, dark theme, selected repository, language menu open, browser preference and supported languages visible.

## Step-linked audit

1. **Open the language control — healthy.** The globe button remains in the header, keeps the current route visible, and has an accessible current-language name.
2. **Browse Recent and All languages — healthy.** Search, browser-preference labeling, dual current/native names, one-to-three country flags and selected state are visible.
3. **Use the menu at the target desktop viewport — healthy.** The anchored panel stays inside the viewport and preserves the existing Console surfaces.
4. **Change language and preserve work — healthy.** Fresh rendered verification covers route preservation, a filled dialog draft, focus restoration, reload persistence, browser-language reset, failed-fragment fallback and narrow layout.

## Findings and accepted deviations

- No actionable P0, P1 or P2 finding remains in the selector journey.
- The implementation menu is wider than the source mockup because the owner added native labels, browser-preference status and up to three flags per language. This is an intentional requirement-backed deviation.
- The current Console uses its approved responsive hamburger shell at this viewport, while the generated source shows inline navigation. This preserves `DC2-2026-08-30-CONSOLE-NAVIGATION-SHELL` and the existing responsive navigation decision; no new navigation system was introduced.
- The underlying Plan data differs from the illustrative mockup because the implementation renders real fixture/API data. This is expected and preserves truthful product behavior.

## Required fidelity surfaces

- **Typography:** existing Inter/system fallback, compact header hierarchy, native-script labels and accessible names remain readable.
- **Spacing:** right-aligned menu, bounded viewport placement, scrollable results and compact header controls remain contained.
- **Colors/tokens:** existing light/dark Console tokens are reused; selected rows and focus rings retain semantic contrast.
- **Assets:** flags are locally hosted licensed SVG assets; the existing Tabler globe icon is used; no raster or CSS-drawn replacement was introduced.
- **Copy/content:** current and native names, browser preference, fallback state and country names are product messages; repository names, task text, comments, logs and evidence remain user data.

## Evidence

- Fresh rendered localization verification: 201 checks, zero failures for Ukrainian at desktop, tablet and mobile themes, including modal-draft preservation.
- Fresh rendered verification artifacts exist for the earlier locale batch, but the 2026-09-24 content review found mixed-language messages in those catalogs and withdrew them from the menu pending rewrite. Current admission is limited to Ukrainian and Czech (313 checks each, zero failures) plus English.
- Complete Console state matrix: 1,421 checks, zero failures after making translated checks case-insensitive and adding the initial loading surface.
- Focused locale/runtime tests: 12 tests passed; edge suite: 22 tests passed.
- Final exact-size audit journey: 79 checks, zero failures at 1487 × 1058 after the Health merge.
- Evidence limits: screenshots cannot prove full accessibility compliance; keyboard, persistence, error recovery and draft preservation are covered by the rendered interaction checks.

## Remaining rollout

Historical rollout snapshot: English, Albanian, German, French, Dutch, Czech, Danish, Swedish, Italian, Norwegian Bokmål, Norwegian Nynorsk, Polish, Portuguese, Spanish, Ukrainian, Russian, Simplified Chinese, Traditional Chinese, Japanese, Korean, Bulgarian, Bosnian, Croatian, and Serbian Latin/Cyrillic were enabled in that interim snapshot. Montenegrin Latin/Cyrillic were then withdrawn after donor-language defects were found. The final current-state result is recorded at the top of this report.

---

# Performance page design QA

final result: passed

Reviewed September 22, 2026 against displayed option 3, retained sketch sbc120f9c414a685f in batch k611f361cacec5556 (decision DC2-PERFORMANCE-PAGE-OPTION3-20260922).

## Comparison and findings

Source visual truth: console/design-reference/performance-option-3.png, also retained by Coordinator. The source and /var/tmp/dc2-performance-work/selected-reference-comparison-3.png were opened together as a paired comparison at 1487 × 1058 pixels, CSS viewport 1487 × 1058, density 1. Both represent the light theme with a selected review. The fixture numbers differ deliberately from the concept; they are test examples, not live measurements.

The final layout preserves the two token distributions, outcome composition table, review history and adjacent reader. The existing repository sidebar and navigation remain as required by the implementation plan. Overall repository totals remain visible while the selected review changes chart scope. These approved product constraints account for the additional repository context and separate overview/review totals.

- Typography: existing Inter, compact operational weights and sizes, readable hierarchy, and intact wrapping. Exact numerical values are available alongside the infographics.
- Spacing: aligned wide-screen summary, ruled outcome/history rows, and bounded panels. Narrow outcome rows use the available width; the reader follows its selected history row.
- Colors: shared light/dark tokens, teal actions, distinct chart colors, and semantic disposition colors; formal contrast and theme checks pass.
- Assets: existing Console icons and wordmark are retained. Charts are live data graphics, not substitutes for raster artwork. No decorative imagery is required by this design.
- Copy: labels describe repository totals, scoped measurements, dispositions and evidence. Missing attribution and unavailable measurements remain explicit.

No actionable P0/P1/P2 visual finding remains. Full-view captures and the native-resolution chart, row, reader and dialog regions were readable without a separate cropped comparison.

## Iterations and verification

The first comparison found that a separate summary row displaced history. It was compacted beside the heading on sufficiently wide containers. Manual narrow-screen review then found an anonymous table column squeezing outcome names. The table now lays out full-width rows, and the rendered journey asserts a readable outcome-name column. Both fixes were recaptured and reviewed.

Final formal evidence: /var/tmp/dc2-performance-work/formal-light-complete and formal-dark-complete. Twelve exact route/state/viewport cells (overview, selected review, custom dates; 1440 × 1024 and 390 × 844; both themes) passed with zero critical findings. All 24 viewport/full-page screenshots were inspected. Both manual-review manifests contain six passes and zero gaps. The focused interaction pass at 1440, 1239, 927 and 390 pixels passed 203 checks, including focus, range cancellation, pagination, revisions, navigation, failure recovery and access. No page JavaScript error was observed.

Live verification is retained separately in /var/tmp/dc2-performance-work/live-initial.json and the final live verification receipt. The live source has partial coverage; the interface does not convert the missing environment to zero. Existing unrelated screenshot-review regressions are tracked separately as p6eb962256c0f2445, with a canonical-checkout reproduction.

## Implementation checklist

The selected layout, responsive correction, exact-value access, working controls, theme checks and visual review are complete. Publication and live decision-link verification are recorded in the task's delivery evidence.

---

# Console Option 3 design QA

Final result: passed

Reviewed September 5, 2026 against the approved Option 3 light and dark
references. Scope: the shared Console visual language, unified Tests results,
progressive disclosure, and discoverability of retained earlier screenshots.

## Reference comparison

Rendered both themes at the reference size, 1456 × 1080. The implementation
preserves the graphite navigation, teal emphasis, unified ruled results,
expandable run information, and secondary technical disclosures. It deliberately
retains real account controls rather than the reference's illustrative avatar,
uses compact operational typography, and represents actual running/error states
rather than copying the mockup's example outcomes. No “Needs attention” section
is present. Fixture names and counts are isolated to browser tests.

## Verification

- Complete Console interaction suite: 1,681 checks, zero failures.
- Focused theme, disclosure, historical provenance, mobile geometry, and recovery
  suite: 179 checks, zero failures.
- Formal rendered matrix: 15 templates/routes × two themes × desktop/narrow,
  60 cells, zero critical findings. Reviewed all 120 viewport/full-page captures
  in paired contact sheets, with additional full-resolution Tests and narrow
  evidence-viewer inspection. The 90 nonblocking warnings include intentional
  horizontal scrolling in existing dense operational tables.
- Formal performance budget: TTFB below 50 ms and LCP below 800 ms on the local
  fixture/proxy topology. These are test-surface measurements, not public-network
  production latency claims.
- Rust workspace: 443 tests passed. Edge integration and static cache/font
  checks passed. Existing Console interactions remain covered; synthetic fixtures
  do not establish the live repository's screenshot count.

## Evidence and boundaries

Private cold artifacts: `/tmp/dc2-shared-theme-verified-console/report.json`,
`/tmp/dc2-shared-theme-focused-ready/report.json`, and
`/tmp/dc2-shared-theme-formal-ready/manual-review.json`. Reference-aligned rendered
captures are `reference-light.png` and `reference-dark.png` in that formal run
directory. These local artifacts are not application content or a completion
ledger. Deployment and real retained-evidence proof are separate operational
acceptance steps; this report records the frozen source design review.

---

## Prior design reviews

# Grouped workers and available deployment usage design QA

final result: passed

## Comparison target

- Source visual truth: the two owner-marked live Deployments screenshots supplied with this request at 1526 × 876, showing one expander on every worker and a false **Not connected in all environments** Usage value for a project with available measurements.
- Browser-rendered implementation: expanded desktop `/var/tmp/dc2-workers-usage.gpYU0z/formal-web-ui-verification-uiZPGJ/screenshots/cell-0002-grouped-workers-and-available-usage-owner-desktop-viewport.png`, collapsed desktop `cell-0007-grouped-workers-and-available-usage_workers-collapsed-owner-desktop-viewport.png`, and the mobile/breakpoint pairs in the same screenshot directory.
- Desktop CSS viewport: 1526 × 876 at device scale factor 1; mobile: 390 × 844; exact responsive boundary: 619/620/621 × 900. No crop, frame, or density conversion was used.
- State: dark Deployments dashboard with the marked project first, two workers, mapping-pending 24-hour usage that resolves to real measured totals, and an independently expanded sibling repository.
- Full-view comparison: the implementation preserves the repository-level collapse control and complete worker facts, removes every row-level expander, adds one labelled Workers disclosure with a factual count, and replaces the false setup result with the project’s measured usage.
- Focused comparison: expanded and Workers-collapsed states were inspected at all five widths so the collection-level control, summary grid transition, complete worker rows, and sibling isolation remain legible.

## Findings

- No actionable P0, P1, or P2 issue remains.
- Individual worker expanders are gone. One **Workers** disclosure per repository hides or restores every row together and keeps the exact count visible.
- A mapping-pending Usage card begins with **Loading usage…**, resolves the exact authorized project once, and updates only its card to measured totals without repainting the page or moving focus.
- Genuine no-measurement, source-failure, and access-required states remain unchanged and do not trigger a repository detail read.
- Five automated low-initial-visibility warnings reflect complete worker rows continuing below a finite first viewport. Every reviewed screenshot keeps the selected project, measured Usage, Workers heading/count, and disclosure immediately recognizable; no content is clipped, overlapped, or unreachable.

## Required fidelity surfaces

- Fonts and typography: the existing system stack, weights, repository headings, facts, badges, and action labels are preserved. **Workers** and its count use the same compact operational hierarchy as the surrounding section.
- Spacing and layout rhythm: the Workers header creates one calm boundary between the six summaries and the row collection. Desktop stays in the established six-column/worker grid; 620 px and below uses the existing labelled summary and worker layouts.
- Colors and visual tokens: all surfaces, borders, status colours, links, focus treatment, and disclosure controls reuse the Console tokens. No new decorative palette, gradient, or effect was introduced.
- Image quality and asset fidelity: this surface contains no raster imagery. Both disclosure levels use the repository's existing official Tabler chevron assets; there is no handcrafted SVG, CSS icon, emoji, or placeholder.
- Copy and content: **Workers**, the factual count, **Collapse workers**, **Expand workers**, **Loading usage…**, measured tokens/requests, and environment coverage all describe real state. No source identity or private environment detail appears.
- States and interactions: repository and Workers collapse remain independent; pointer, Enter, Space, sibling isolation, same-session rerender preservation, lifecycle controls after restore, pending-to-measured Usage, focus retention, unobserved, source-failed, denied, wide, narrow, and breakpoint variants are exercised through the rendered UI.

## Browser evidence

- Complete merged-source Console matrix: 1,679 checks, zero failures in `/var/tmp/dc2-workers-usage.gpYU0z/merged-console-final/report.json`.
- Focused merged interaction inventory: 333 checks, zero failures in `/var/tmp/dc2-workers-usage.gpYU0z/merged-focused/report.json`.
- Formal run `formal-web-ui-mtmq0rvy-d2740e86`: 10/10 expanded/Workers-collapsed cells at 390, 619, 620, 621, and 1526 px; zero critical findings, five reviewed low-initial-visibility warnings, and passing coverage.
- All twenty final viewport/full-page images were inspected; ten pass decisions and zero gaps were finalized in `/var/tmp/dc2-workers-usage.gpYU0z/formal-web-ui-verification-uiZPGJ/manual-review.json`.
- Browser console and network failures: none in the complete or focused passes.

## Comparison history

1. The owner identified that collapsing one worker had no meaningful dashboard use and that the project’s Usage card contradicted the available Usage destination.
2. The first product pass removed row controls, introduced one Workers disclosure, and hydrated only mapping-pending usage. Its focused test initially read stale populated DOM because identical-hash navigation did not reload; the harness now performs real state reloads, and the merged 333-check interaction pass is green.
3. The first formal run used a stale fixture process whose repository order displaced the target project and forced the action to scroll. A current-source fixture and fresh complete formal run passed; one preceding cold-start TTFB outlier did not repeat.

## Implementation checklist

- [x] Remove every individual worker expander.
- [x] Add one labelled, counted Workers disclosure per repository.
- [x] Preserve independent repository and Workers state through rerenders.
- [x] Resolve available mapping-pending Usage in place without routine per-project scans.
- [x] Preserve genuine missing, failure, permission, privacy, and lifecycle behavior.
- [x] Verify the owner viewport, mobile, and exact responsive boundary.

## Follow-up polish

- No follow-up is required for the two requested dashboard corrections.

---

## Archived prior design QA

# Infinite scrolling and highlighted test-log viewer design QA

final result: passed

## Comparison target

- Source visual truth: the owner-marked live Tests screenshot supplied with this request, plus the same 1095 × 876 current-source reproduction at `/tmp/dc2-infinite-logs-source-1095.png`.
- Browser-rendered implementation: highlighted raw output at `/tmp/dc2-infinite-logs-highlighted-1095.png`, formatted JSON-lines at `/tmp/dc2-infinite-logs-structured-1095.png`, and the 390 × 844 structured mobile view at `/tmp/dc2-infinite-logs-structured-390.png`.
- Source and desktop implementation CSS viewport: 1095 × 876 at device scale factor 1; mobile comparison: 390 × 844 at device scale factor 1. No crop, density conversion, or device frame was used.
- State: dark Console Tests page, retained long raw output with an earlier cursor, plus valid JSON-lines and whole-JSON streams.
- Full-view comparison: `/tmp/dc2-infinite-logs-before-after.png` places the same raw-log source and implementation together. The dialog structure and bounded-log trust boundary are preserved while the manual paging control disappears and useful output gains visual hierarchy.
- Focused comparison: all raw, JSON-lines, and whole-JSON readers were inspected separately at 390, 719, 720, 721, and 1095 px because the toolbar changes composition at 720 px.

## Findings

- No actionable P0, P1, or P2 issue remains.
- The owner-reported manual-paging defect is removed: reaching the top loads the next bounded earlier page automatically, preserves the exact visible line, and eventually shows **Start of output**. Short pages fill only until scrollable or exhausted.
- Raw numbers, timestamps, keys, strings, booleans, failures, warnings, and success terms are visually differentiated without altering the source coordinates. Valid JSON and JSON-lines are indented and receive an explicit format badge.
- Hostile tags and event attributes remain inert literal text. A continuation error leaves already visible output in place and offers one exact retry instead of resetting the reader.
- Five automated visibility warnings describe raw content above the deliberately bottom-positioned newest-output viewport; three scroll-topology warnings describe the expected page plus inner log reader on narrow screens. All related viewport and full-page captures were inspected and show reachable content without clipping or overlap.

## Required fidelity surfaces

- Fonts and typography: the existing system UI hierarchy is preserved; the reader uses the established monospaced face with increased line height, disabled ligatures, stable whitespace, and distinct but non-decorative syntax colours.
- Spacing and layout rhythm: the output remains the dominant dialog region. Search and failure actions stack at 720 px and below, return to an inline toolbar at 721 px, and never displace or cover the reader.
- Colors and visual tokens: syntax accents reuse the Console's blue, green, amber, red, cyan, violet, and muted text tokens on the existing navy surface. Every sampled token class passed contrast measurement; there is no gradient or new decorative language.
- Image quality and asset fidelity: the log reader contains no raster imagery and introduces no icon asset, handcrafted SVG, CSS drawing, emoji, or placeholder.
- Copy and content: **Start of output**, **Jump/Refresh latest**, the format badges, stable line ranges, and one retry state describe real reader state. There is no paging instruction or numeric coordinate field in the ordinary journey.
- States and interactions: top-boundary loading, repeated continuation, short-page fill, anchor preservation, latest return, search/failure continuation, stream switching, active refresh, continuation failure/retry, live-row replacement, close/focus return, structured formatting, hostile markup, and responsive variants are exercised through the rendered UI.

## Browser evidence

- Complete frozen-candidate Console matrix: 1,672 checks, zero failures in `/tmp/dc2-infinite-logs-console-final-merged/report.json`.
- Focused merged Tests/log interaction inventory: 326 checks, zero failures in `/tmp/dc2-infinite-logs-merged-focused/report.json`.
- Formal run `formal-web-ui-mtm3f78l-c39ddd7e`: 15/15 exact raw/JSON-lines/whole-JSON cells at 390, 719, 720, 721, and 1095 px; zero critical findings, eight reviewed intentional scroll/initial-position warnings, and passing coverage.
- All thirty final viewport/full-page images were inspected; fifteen pass decisions and zero gaps were finalized in `/tmp/formal-web-ui-verification-TVedif/manual-review.json`.
- Browser console and network failures: none in the complete or focused passes.

## Comparison history

1. The source required a visible **Load earlier output** action and rendered every line as flat monochrome text.
2. The first continuous-reader implementation exposed a real anchor bug: prepending relative to scroll height could shift the exact line being read. The reader now measures the first visible row before the request and restores that same DOM anchor after insertion.
3. The combined live-refresh pass found focus sampling could race a Tests rerender. Close now resolves the current replacement action by run identity, and verification waits for the observable focus result. A fresh complete Console pass and final formal review found no remaining P0/P1/P2 issue.

## Implementation checklist

- [x] Remove the earlier-output paging control from every reader state.
- [x] Load bounded earlier and result pages at the appropriate scroll boundary.
- [x] Preserve the exact reading anchor and avoid duplicate/cascaded requests.
- [x] Highlight useful raw tokens and format valid JSON/JSON-lines.
- [x] Escape every source token and preserve stable source-line ranges.
- [x] Keep latest, search, failure, refresh, retry, and focus recovery working.
- [x] Verify mobile, desktop, and the exact responsive boundary.

## Follow-up polish

- No follow-up is required for the requested log-reader correction.

---

## Archived prior design QA

# Collapsible deployment dashboard design QA

final result: passed

## Comparison target

- Source visual truth: the four owner-marked live Deployments screenshots supplied with this request, plus the same-state current-source reproduction at `/tmp/dc2-collapse-dashboard-source-1095.png`.
- Browser-rendered implementation: expanded `/tmp/dc2-collapse-dashboard-focused-2/deployments-1095.png`, repository collapsed `/tmp/dc2-collapse-dashboard-focused-2/deployments-1095-repository-collapsed.png`, deployment collapsed `/tmp/dc2-collapse-dashboard-focused-2/deployments-1095-deployment-collapsed.png`, and mobile equivalents in the same directory.
- Source and implementation CSS viewport: 1095 × 876 at device scale factor 1; mobile comparison: 390 × 844 at device scale factor 1. Full-page heights differ because the added factual summary rows make the expanded implementation taller and collapsing makes selected sections shorter; width and density were not normalized or cropped.
- State: dark Console Deployments dashboard with two repository groups, healthy/degraded deployments, expanded Test and Health summaries, plus each requested collapsed state.
- Full-view comparison: `/tmp/dc2-collapse-dashboard-before-after.png` places the same-state source and expanded implementation together. The existing repository-first structure, status hierarchy, operational color, responsive grid, links, and lifecycle controls are preserved; only the requested density controls and factual Test/Health detail are added.
- Focused comparison: the repository header, individual deployment header, Tests card, and Health card were inspected in the owner-reported viewport. Separate 390 px and 619/620/621 px captures verify the changed stacked composition and collapse controls where the full-view comparison is too small to judge precisely.

## Findings

- No actionable P0, P1, or P2 issue remains.
- Repository collapse keeps the repository name, identity, deployment count, overall condition, and restore control visible. It hides only that repository's summaries and deployments; every sibling remains unchanged.
- Deployment collapse keeps the deployment name, immutable identity, state/health badges, and restore control visible. Routing facts and lifecycle actions return on expansion.
- Tests now shows the selected run's proof type, start recency, tier, elapsed time, and combined observed output. Health now shows current CPU, memory, storage, and deployment count. Missing and restricted data produce no synthetic details.
- Ten formal warnings report low initial visibility for the intentionally compact collapsed region. The reviewed screenshots show the selected identity/status row exactly as requested; no content is clipped, obscured, or unreachable.

## Required fidelity surfaces

- Fonts and typography: the existing system font stack, weights, monospaced identities, compact facts, wrapping, and operational hierarchy are preserved. New facts use smaller optical weight than the primary status and remain readable at every sampled width.
- Spacing and layout rhythm: the 1095 px summary remains a balanced 3-by-2 grid; Test and Health use an aligned two-column fact grid. Collapse toggles align at the right edge of repository headers and beside deployment identity. Mobile uses labelled stacked sections with 38 px controls and no document overflow.
- Colors and visual tokens: all new controls and facts reuse the Console's navy/slate panels, cool borders, blue focus/link color, green healthy/running, amber attention, and red unhealthy tokens. No decorative palette or new visual language was introduced.
- Image quality and asset fidelity: this dashboard contains no raster imagery. Collapse controls use the repository's existing official Tabler chevron assets; no handcrafted SVG, CSS drawing, emoji, gradient, or placeholder was introduced.
- Copy and content: labels state the real concepts—Tier, Elapsed, Output, CPU, Memory, Storage, and Deployments—and every value comes from the existing repository-attributed APIs. Collapse labels include the exact project or deployment for assistive technology.
- States and interactions: repository and deployment expand/collapse, mouse, Enter, Space, independent sibling state, same-session rerender preservation, lifecycle actions after re-expansion, summary continuations, missing/restricted facts, and responsive variants are exercised through the rendered UI.

## Browser evidence

- Complete frozen-candidate Console matrix: 1,659 checks, zero failures in `/tmp/dc2-collapse-dashboard-console-release/report.json`.
- Focused interaction inventory: 315 checks, zero failures in `/tmp/dc2-collapse-dashboard-focused-2/report.json`.
- Formal run `formal-web-ui-mtm0yz1n`: 15/15 exact expanded/repository-collapsed/deployment-collapsed cells at 390, 619, 620, 621, and 1095 px; zero critical findings, ten reviewed intentional compact-region warnings, and passing coverage.
- All thirty final viewport/full-page screenshots were inspected; fifteen pass decisions and zero gaps were finalized in `/tmp/formal-web-ui-verification-qDKHVa/manual-review.json`.
- Browser console and network failures: none in the complete or focused passes.

## Comparison history

1. The source and owner comments identified four gaps: repositories could not collapse, individual local deployments could not collapse, and Tests/Health left useful API-backed facts out of large summary cards.
2. The first implementation added independent controls and factual details. Focused desktop/mobile review found the structure readable and all interactions working.
3. The first formal run found that icon-only toggles were not explicitly recognizable as continuation anchors. The exact controls received the verifier's semantic anchor marker. A fresh complete run passed; the remaining low-visibility warnings are the deliberate effect of collapsing, confirmed in all thirty screenshots.

## Implementation checklist

- [x] Collapse and expand each repository independently.
- [x] Collapse and expand each managed or observed deployment independently.
- [x] Preserve identity and condition while detail/actions are hidden.
- [x] Keep collapse choices through same-session page rerenders.
- [x] Add factual Test and Health detail without new backend or cross-repository data.
- [x] Preserve every continuation and lifecycle control when expanded.
- [x] Verify wide, mobile, and exact responsive-boundary states.

## Follow-up polish

- No follow-up is required for the four requested dashboard changes.

---

## Archived prior design QA

# Readable test-log viewer design QA

final result: passed

## Comparison target

- Source visual truth: `/tmp/dc2-log-reader-before-dialog.png`, a fresh 1095 × 876 reproduction of the owner-marked live dialog. It matches the supplied browser evidence: no output is shown, technical metadata dominates, and reading an exact portion requires numeric inputs.
- Browser-rendered implementation: `/tmp/dc2-log-reader-final-1095.png`; narrow implementation: `/tmp/dc2-log-reader-final-390.png`.
- Source and desktop implementation pixels/CSS viewport: 1095 × 876 at device scale factor 1. Narrow pixels/CSS viewport: 390 × 844 at device scale factor 1. No crop, density conversion, or device frame was used.
- State: dark Console Tests page, first run's retained error-output stream, log dialog open, newest output loaded, technical details collapsed.
- Full-view comparison: `/tmp/dc2-log-reader-before-final.png` places the same-state source and final implementation together. The final view preserves the Console shell and tokens while replacing the narrow metadata/form card with a large text-first reader.
- Focused comparison: the full-view pair keeps all log controls and output legible at original resolution. The narrow capture and the exact 719/720/721 formal captures were reviewed separately because the toolbar changes composition at that boundary.

## Findings

- No actionable P0, P1, or P2 issue remains.
- The source's P1 workflow defect is removed: opening Logs now catalogues the exact stream and immediately loads its newest bounded text. No line, byte, start, end, or numeric range field remains in the ordinary Console.
- Technical hash, timing, retention-depth, and structured-evidence facts remain available under **Stream details** instead of preceding the output.
- The first corrected narrow layout left half of the action row unused while **Show likely failure** occupied the other half. That P2 imbalance was fixed: the lone action now fills the row and shares it only when **Jump/Refresh latest** is actually present.

## Required fidelity surfaces

- Fonts and typography: the existing system UI stack, weights, and hierarchy are preserved; stream output uses the established monospaced log style with readable line height. Labels wrap or reflow without truncation at the checked widths.
- Spacing and layout rhythm: the desktop dialog uses the available viewport for the reader, while mobile keeps compact controls above a large output region. The modal remains contained at 390, 719, 720, 721, and 1095 px with no clipped action or document overflow.
- Colors and visual tokens: navy/slate surfaces, cool dividers, focus blue, muted metadata, and high-contrast output reuse the existing Console tokens. The final formal pass found no contrast, theme, overlap, clipping, or scroll-topology warning.
- Image quality and asset fidelity: this surface contains no raster, illustration, logo, or new icon asset. No placeholder, handcrafted SVG, CSS drawing, gradient, emoji, or text-glyph substitute was introduced.
- Copy and content: stream labels now describe **Error output** and **Standard output** rather than repeating internal phase names. **Load earlier output**, **Jump/Refresh latest**, **Search**, and **Show likely failure** state the user's action directly. Real metadata and log text remain API-backed.
- States and interactions: automatic newest output, progressive prepend with preserved position, return to latest, literal search pagination, likely-failure context, stream switching, active refresh, empty state, expired-read retry, dialog close/focus return, and narrow reflow were exercised through the rendered interface.

## Browser evidence

- Complete frozen-candidate Console matrix: 1,641 checks, zero failures in `/tmp/dc2-readable-logs-console-final/report.json`.
- Focused interaction inventory: 299 checks, zero failures in `/tmp/dc2-readable-logs-focused-final/report.json`.
- Final formal run `formal-web-ui-mtlvv4t9`: 5/5 exact cells at 390, 719, 720, 721, and 1095 px; zero critical findings, zero warnings, and passing coverage.
- All ten final viewport/full-page images were inspected; five pass decisions and zero gaps were finalized in `/tmp/formal-web-ui-verification-nly2pg/manual-review.json`.
- Browser console and network failures: none in the complete and focused Console passes.

## Comparison history

1. The source audit found a P1 task-flow failure: the dialog led with a dense ten-fact grid, showed no output, duplicated internal stream labels, and required users to choose retrieval mechanics or type numeric coordinates.
2. The first implementation made output automatic, widened the reader, collapsed details, humanized stream names, and replaced coordinate entry with bounded plain-language actions. Focused recovery testing then found and removed an unrelated page-level retry inherited from the shared error component.
3. The first responsive comparison found a P2 half-empty action row at 720 px and below. The lone failure action now spans the row; the final same-state desktop/mobile comparison and fresh formal pass found no remaining P0/P1/P2 issue.

## Implementation checklist

- [x] Show newest readable output immediately on open and stream change.
- [x] Remove numeric line/byte range entry from the Console reading journey.
- [x] Keep bounded older-output, latest, literal-search, and likely-failure controls working end to end.
- [x] Preserve stable coordinates, untrusted-output labelling, access boundaries, and exact API selectors.
- [x] Cover short, long, active, empty, expired, failed, multi-stream, wide, narrow, and breakpoint states.
- [x] Pass the complete Console suite, formal geometry/contrast/performance checks, and finalized manual review.

## Follow-up polish

- No follow-up is required for the requested log-reading correction.

---

## Archived prior design QA

# Visual test evidence review design QA

final result: passed

## Comparison target

- Source visual truth: `/home/DevCoordinator2/console/design-reference/test-evidence-review-option-3.png`, the third displayed Product Design direction selected by the owner.
- Browser-rendered implementation: `/tmp/dc2-visual-evidence-console-release/test-evidence-review-feedback-wide.png`.
- Source pixels: 1487 × 1058. Implementation pixels and CSS viewport: 1440 × 1024 at device scale factor 1. Their aspect ratios differ by less than 0.1%, so the source was judged proportionally at the implementation viewport without crop or frame distortion.
- State: dark Console, failed UI test run, selected invalid-password journey step, desktop viewport capture, one open feedback thread, and visible pin/rectangle/arrow annotations.
- Full-view comparison: both artifacts were opened together at original resolution. The final implementation preserves the selected three-zone hierarchy, dominant screenshot, ordered journey rail, compact run context, complete tool strip, capture/finding inspector, annotation discussion, and viewport comparison.
- Focused comparison: the original-resolution full views keep the toolbar, journey rows, capture facts, annotations, comment actions, and official icons legible; separate crops were not needed.

## Findings

- No actionable P0, P1, or P2 difference remains.
- The generated concept includes an **Export evidence bundle** control. It is intentionally absent because the owner requested viewing and commenting, not export; adding an inert or unrequested export path would be false product behaviour.
- The source shows thirteen synthetic steps and three separate comments. The implementation renders the real manifest count and uses one end-to-end fixture thread with three marks; production content grows from factual test evidence rather than invented rows.
- The implementation uses the existing Console's text status badges instead of generated checkmark glyphs and keeps the established global header. This is deliberate product-system fidelity, not visual drift.

## Required fidelity surfaces

- Fonts and typography: the existing system font stack, weights, compact labels, monospaced run identity, truncation, and wrapping match the Console. Formal checks found no measurable text-contrast failure after the first correction.
- Spacing and layout rhythm: desktop matches the selected left/canvas/right proportions. Run facts were compressed into the header so the screenshot begins near the source position. At 1180 px and below, the three-column board becomes a horizontal journey picker plus a non-overlapping feedback tray; exact 1179/1180/1181 and 559/560/561 samples pass.
- Colors and tokens: navy/slate surfaces, cool dividers, blue selection, green pass, amber review, red failure, and annotation colours reuse the product tokens. There are no gradients or decorative effects absent from the Console.
- Image quality and asset fidelity: evidence PNGs retain their native aspect ratio and become zoomable rather than cropped. All interface icons are official Tabler assets under the repository's existing license; no inline or handcrafted substitute is used.
- Copy and content: every visible run, route, state, viewport, time, finding, and comment comes from the API or user input. Missing, invalid, expired, tampered, and unauthorized evidence use honest states.
- States and interactions: select/move/resize/delete, pin, rectangle, arrow, freehand, highlight, text, colour, undo/redo, zoom, fit, Space-pan, clear, step/capture/viewport switching, finding focus, task creation, replies, edits, Plan continuation, resolve/reopen, deletion, and the mobile feedback sheet are all exercised through the rendered UI.

## Browser evidence

- Complete current-source Console matrix: 1,625 checks, zero failures in `/tmp/dc2-visual-evidence-console-release/report.json`.
- Final focused interaction inventory: 283 checks, zero failures in `/tmp/dc2-visual-evidence-design-final/report.json`.
- Final formal run `formal-web-ui-mtkn9iwc`: 8/8 exact cells at 390, 559/560/561, 1179/1180/1181, and 1440 px; zero critical findings and passing coverage.
- All sixteen final viewport/full-page images were inspected; eight pass decisions were finalized with zero gaps in `/tmp/formal-web-ui-verification-jdsp9G/manual-review.json`.
- Browser console and network failures: none in the complete and focused Console passes.

## Comparison history

1. The first formal capture exposed measurable low-contrast metadata, nested scrolling, collapsed-sheet occlusion, toolbar clipping, and document overflow. The palette, mobile scroll ownership, toolbar sizing, and tray layout were corrected before review.
2. The first visually clean breakpoint set still made the three-column canvas too narrow at 901 px. The full desktop board now begins above 1180 px; 1180 and below use the readable tablet/mobile composition.
3. The first source comparison showed a separate four-item run-facts row pushing the screenshot below the source hierarchy. Tier, readiness, and recency moved into the compact run line. The final same-state comparison found no remaining P0/P1/P2 mismatch.

## Implementation checklist

- [x] Match the owner-selected review-board hierarchy and existing Console design system.
- [x] Keep factual screenshots dominant and immutable.
- [x] Make every visible review and annotation control work end to end.
- [x] Turn top-level screenshot suggestions into ordinary Plan feedback.
- [x] Preserve desktop, tablet, mobile, empty, error, permission, tamper, and retention behaviour.
- [x] Pass formal geometry/contrast/performance checks and finalized manual image review.

## Follow-up polish

- No follow-up is required for the requested page.

---

## Archived prior design QA

# Repository deployments dashboard design QA

final result: passed

## Comparison target

- Source visual truth: `/home/DevCoordinator2/console/design-reference/deployments-repository-dashboard.png`
- Normalized source comparison: `/tmp/dc2-deployments-target-1440x1024.png`
- Browser-rendered implementation: `/tmp/dc2-deploy-dashboard-formal-final/screenshots/cell-0004-repository-deployments-dashboard-desktop-viewport.png`
- Focused legacy-repository comparison: `/tmp/dc2-deployments-target-legacy-focus.png` and `/tmp/dc2-deployments-implementation-legacy-focus.png`
- Focused managed-repository comparison: `/tmp/dc2-deployments-target-repo-focus.png` and `/tmp/dc2-deployments-implementation-repo-focus.png`
- Source pixels: 1487 × 1058, proportionally normalized to 1440 × 1024 for comparison.
- Implementation pixels and CSS viewport: 1440 × 1024 at device scale factor 1.
- State: populated dark Console fixture with one healthy observed repository and one managed repository containing a running deployment and a degraded long-content deployment.

## Browser evidence

- The complete Console matrix passed 1,493 checks with zero failures in `/tmp/dc2-deploy-dashboard-final-browser2/report.json`.
- The focused repository-dashboard inventory passed 229/229 checks across 320, 390, 430, 619/620/621, 834, 959/960/961, 1179/1180/1181, 1239/1240/1241, and 1440 px in `/tmp/dc2-deploy-dashboard-interactions-final/report.json`.
- Primary interactions tested: every repository-specific Plan, Progress, Codex Usage, and Decisions continuation; repository-labelled Tests and Health continuations; domain editing; managed and observed lifecycle actions; apply-state disabling; and exact post-action API reads.
- Attribution fixtures deliberately give the two repositories different plan, progress, usage, test, health, and decision states. Assertions prove that no deployment or summary value appears in the other repository section.
- Empty, loading, error, permission-limited, applying, observed, degraded, long-content, missing-data, and partial-usage states remain covered by the complete matrix.
- Browser console errors in the final comparison: none.
- Formal run `formal-web-ui-mtit1v0d` checked 16 exact cells covering desktop, tablet, two mobile sizes, and ±1/at samples around 620, 960, 1180, and 1240 px. It reported zero critical findings and passed coverage.
- All 16 initial-viewport/full-page pairs were manually reviewed and finalized with zero gaps in `/tmp/dc2-deploy-dashboard-formal-final/manual-review.json`.
- Three formal warnings occurred when a below-fold domain-edit control landed exactly on the 960 px viewport edge. The reviewed full-page evidence shows each control fully visible and reachable through normal document scrolling; no element covers it.

## Findings

- No actionable P0, P1, or P2 difference remains.
- The generated target used an ambiguous copy-like glyph beside domains. The implementation deliberately uses the existing small text button labelled `edit`, because the real action changes the routed domain and must not imply copying. This is a truthful interaction refinement, not design drift.
- The implementation adds a small `3 of 4 environments included` line beneath partial Codex usage. The generated target omitted completeness, but the established Console contract requires missing usage to remain visible and never look complete. The note stays visually subordinate.
- Optional P3: the implementation keeps the existing Console shell's slightly denser typography and filled small-button treatment instead of enlarging the global shell to match ImageGen. This preserves the approved cross-page navigation system and does not weaken the selected page hierarchy.

## Required fidelity surfaces

- Fonts and typography: both views use the Console system UI stack with the same strong repository headings, compact labels, readable values, blue continuations, and monospaced repository/deployment identities. Long names and domains wrap without truncation or collision.
- Spacing and layout rhythm: desktop preserves two full-width repository sections, six aligned summary columns, status rails, and five operational deployment zones. The summary switches to 3-by-2 before the shell becomes compact; deployment facts switch at 1180 and 960 px; mobile uses labelled stacked rows. No document-level horizontal scrolling, clipped value, or off-canvas control remains.
- Colors and visual tokens: the implementation preserves the source and product's navy/slate surfaces, cool dividers, blue links, green running/healthy treatment, amber degraded/attention treatment, red unhealthy treatment, and muted evidence metadata. Formal contrast and declared-dark-theme checks produced no critical finding.
- Image quality and asset fidelity: the target contains no raster content or decorative illustration. The implementation uses the existing Tabler arrow asset for continuation links and adds no placeholder, handcrafted SVG, CSS illustration, gradient, or fake icon.
- Copy and content: every visible number and state maps to the fixture/API contract. Missing plan, progress, usage, tests, and decisions remain explicit; partial usage names its incompleteness; no global summary mixes repositories; the removed declared-only side area does not return.
- States and interactions: all enabled controls call existing real commands and re-read state. Viewer/operator/administrator differences, observed-only behavior, applying-state disabling, failure recovery, and repository-scoped navigation remain honest.

## Comparison history

1. The first implementation comparison found a P2 density mismatch against the approved desktop mock: headings, summary values, and deployment rows were too compact. Typography, vertical rhythm, and record spacing were increased while retaining the established Console shell.
2. The first formal pass found a P1 tablet clipping defect: the four-column deployment grid extended 70 px beyond an 834 px repository section. The two-column tablet transition moved to 960 px, and exact 959/960/961 samples now prove all fields and controls remain contained.
3. Focused visual review found a P2 summary-density issue just above 1180 px: six repository summary columns forced long repository-labelled links into cramped wraps. The summary transition now aligns with the 1240 px navigation boundary, while the independent deployment grid keeps its 1180 px boundary. Exact 1179/1180/1181 and 1239/1240/1241 samples pass.
4. The final full-view and focused source-to-render comparisons found no remaining actionable mismatch. The complete Console matrix, formal geometry/contrast pass, and immutable manual-review manifest all pass on the final source.

## Implementation checklist

- [x] Match the selected repository-centred desktop hierarchy and Console visual system.
- [x] Keep each repository's plan, progress, usage, tests, health, decision, and deployments strictly attributed.
- [x] Continue to the exact repository-specific destinations where those routes exist.
- [x] Preserve every real lifecycle and domain action without inventing dashboard mutations.
- [x] Show missing, partial, restricted, and failed evidence truthfully.
- [x] Pass desktop, tablet, several mobile sizes, every responsive boundary, full Console regression, formal verification, manual visual review, and source-to-render design QA.

## Follow-up polish

- No follow-up is required for the requested dashboard.

---

# Design QA: Combined Sketch Review and Annotation Workspace — 2026-09-23

final result: passed

## Audit scope

This audit covers the selected Option 3 visual direction, “Annotation-first comparison tray,” and the implemented Sketch set review route:

- Source target: [approved Option 3](design-qa/sketch-annotation/approved-option-3.png)
- Implementation capture: [rendered Option 3 state](design-qa/sketch-annotation/implementation-option-3-1263x1246.png)
- Paired comparison: [source and implementation side by side](design-qa/sketch-annotation/comparison-option-3.png)
- Viewport: 1263 × 1246 CSS pixels, device scale factor 1
- Route/state: Sketches → set review → Option 3 active
- Theme: existing Console light shell with dark retained review content
- Source decision: `DC2-SKETCH-ANNOTATION-WORKSPACE-SELECTED-20260923`

The source target was opened and inspected before comparing it with the rendered implementation. The implementation was captured from the Console code through a local edge and daemon fixture that served retained sketch images and exercised the same annotation and decision APIs.

## User goal and accessibility target

The owner needs to compare all three generated directions, mark the active image, write comments that attach to the active or selected options, and change each option’s Keep/Reject/Undecided decision without losing the set context.

The audit target is a keyboard-operable, readable review flow with visible focus, clear per-image state, usable mobile reflow, and no hidden or inert core controls.

## Strengths

- The active image remains central while the bottom tray keeps all three options available.
- The selected option, retained annotations, and per-image decision are visible together.
- The annotation toolbar uses the existing evidence tools and preserves the project’s established visual language.
- Comments and decisions stay in the same review route; switching options does not leave the set.
- The mobile layout keeps the three-option tray and exposes a bottom capture-details control.
- The source and implementation share the same hierarchy: set context, active review canvas, comments/inspection, and all-option comparison.

## UX and accessibility audit

The combined Product Design audit found no actionable P0, P1, or P2 findings in the reviewed states.

- Task entry: the existing “Review set” action opens the combined workspace directly.
- Hierarchy: the active sketch is the largest surface; the option tray is secondary but persistent.
- Interaction continuation: opening a set focuses the annotation canvas; switching an option updates the active title, image, annotations, and decision controls.
- Persistence: saved marks and comments return through the retained annotation API; decisions use revision-checked writes.
- Keyboard behavior: the canvas is focusable, the inspector has a responsive toggle, and the existing decision controls remain keyboard targets.
- Responsive behavior: the formal verifier checked 624 × 928 and 1280 × 900 required cells.
- Contrast and geometry: the formal verifier found 0 critical findings across all 4 cells. It retained only 3 non-blocking media/theme warnings for the image-heavy review surface.

## Interaction evidence

The fresh end-to-end browser journey passed at 624 × 928 and 1280 × 900:

1. Loaded all three retained images in the Sketches collection.
2. Opened the combined set annotation workspace.
3. Confirmed the annotation toolbar, active canvas, inspector, three option tiles, and three decision controls.
4. Drew a rectangle mark and posted a comment through the rendered form.
5. Switched to another option while preserving the set route.
6. Changed the active option decision and observed the revision-backed result.
7. Completed the journey with no browser or page errors.

The focused Rust sketch lifecycle test also passed. The current retry used the designated bulk target directory after the first attempt ran out of `/tmp` space during linking.

## Comparison notes

The implementation retains the established Console repository shell around the selected mockup direction. That shell is an intentional project-context requirement; the selected visual direction is applied to the set review workspace inside it. The source mockup uses illustrative sample images, while the implementation renders the retained sketch bytes supplied by the Coordinator.

No P0, P1, or P2 visual or interaction difference remains.

## Evidence and limits

- Formal Web UI run: `formal-web-ui-muer939z-f8d6fcca`
- Formal result: 4/4 required cells passed, 0 critical findings
- Manual screenshot review: `/tmp/dc2-sketch-annotation-manual-review-latest.json`
- Browser journey fixture: `/tmp/dc2-sketch-annotation-e2e.mjs`
- The audit used a local Coordinator edge/daemon fixture for safe real API and persistence behavior. The public authenticated Console tab was not mutated during verification.
- Full WCAG conformance is outside a screenshot audit; keyboard and geometry checks were exercised, while assistive-technology output requires a dedicated screen-reader pass.

## Final checklist

- [x] Approved source target opened and inspected
- [x] Matched implementation screenshot captured
- [x] Side-by-side comparison captured
- [x] UX and accessibility review completed
- [x] Mobile and desktop states checked
- [x] Rendered annotation, comment, and decision journey passed
- [x] No actionable P0/P1/P2 findings remain
- [x] Final result: passed


## API-equivalent Repository Cost Pulse audit (2026-09-27)

### Repository Usage Cost Pulse audit

Date: 2026-09-27 UTC

Source visual truth: built-in Image Gen asset `exec-278394c5-83d4-47be-89f1-d4b500b94cb4.png` (the user-selected displayed option 3, `c76ec26b299747fb96512b5b25e7b234767901634b4802d95373db1ec4f966ee`). The retained source is outside this checkout; the Coordinator decision keeps its identity and hash.

Implementation captures:

- `/var/tmp/dc2-api-equivalent-cost/hold-run-4/wide.png` — 1487×2056 pixels, CSS viewport 1487×1058, dark Console surface.
- `/var/tmp/dc2-api-equivalent-cost/hold-run-4/narrow-2.png` — 390px CSS viewport, light theme, with monetary columns retained in compact tables.
- `/var/tmp/dc2-api-equivalent-cost/design-qa-20260927/source-vs-final.png` — combined source and final rendered comparison input inspected during review.

The fixture route was `#/usage/r0123456789abcdef` with populated repository data, the authenticated owner state, the same repository Usage journey, and the API-equivalent cost, model, activity, outcome, and coverage content enabled. The source and implementation were inspected at the same wide viewport and matched for the dark comparison; the narrow capture checked responsive reflow separately.

## Steps

1. **Open repository Usage.** The repository identity and Usage destination are the first substantial content. The route loads its real usage response and preserves the existing repository navigation. **Healthy.**
2. **Read the cost summary.** The top band keeps provider-reported tokens primary and places the API-equivalent estimate beside model requests and cache efficiency. The value is explicitly labelled as an estimate. **Healthy.**
3. **Compare work phase and model cost.** The phase trend remains visible, and the model panel adds a cost mix, exact token/request counts, average cost per request, and coverage states. **Healthy.**
4. **Inspect outcome and activity attribution.** Outcome cost pulse ranks attributed work; activity rows show tokens, estimate, and share; unavailable rows remain visibly unavailable. **Healthy.**
5. **Inspect timing, tool outcomes, and completeness.** Existing timing/tool sections remain below the cost surfaces, and the completeness disclosure retains subset and rate-card explanations. **Healthy.**
6. **Check narrow layout and theme.** The 390px capture keeps the repository header, metrics, model donut, outcome table, activity breakdown, timing, tools, and completeness readable without page-wide overflow. **Healthy.**

## Findings

No actionable P0, P1, or P2 findings remain. The model mix now sits beside the phase trend at the wide target width, and narrow tables retain the monetary estimate column while collapsing secondary token/coverage columns.

P3: The existing product header and repository identity remain more compact than the generated target. This intentionally preserves established Console navigation and does not affect the selected Cost Pulse hierarchy.

## Interaction evidence

`CONSOLE_VERIFY_WORKSPACE_ONLY=1` completed 389 rendered checks with zero failures. The run exercised the Usage route at wide and narrow viewports, light and dark themes, range controls, repository navigation, coverage disclosure, cost panels, loading/unavailable states, and the existing refresh/error journeys. Fresh final captures also verified the wide two-panel trend/model composition, compact model/outcome tables, and no document overflow at 390px. JavaScript syntax checks passed for `console/app.js` and `console/verify.mjs`.

## Comparison history

The first implementation comparison identified the missing model cost surface and a wide-layout mismatch relative to the selected option. The backend model-cost projection, donut/legend, exact model table, request-average values, side-by-side wide layout, and compact responsive columns were added. Fresh wide and narrow captures were taken under the same route and populated state; the revised comparison is the evidence above.

## Evidence limits

The source visual is a design target rather than a live billing statement. The audit verifies rendered hierarchy, copy, responsive behavior, and interaction evidence; it does not prove provider billing or subscription spend. The live production Console was not changed from this worktree because DevCoordinator2's non-self-hosting boundary requires its own reviewed candidate workflow.

final result: passed

## Performance Cost Matrix 2026-09-28

### Source and comparison

- Source visual truth: retained image `exec-93dc9ba2-00c7-4b7a-92d8-bf92f2b54448.png`, Coordinator sketch `s038ac45e937b98bb`, selected displayed option 3, SHA-256 `d763e761390f658e7d0ea85b55e01130e4830e423bd2782f34a4a3000af57827`.
- Implementation route: `https://console.vr.ae/#/performance/r2c68ddac83a3140a`, hdlripper, authenticated owner state, 7-day window, partial coverage from one of two configured collectors.
- Wide implementation capture: `/home/DevCoordinator2/target/native-console-performance-1790601871362-6e37ff/performance-wide.png` (1440px CSS viewport, full-page 1440×3719 capture, device scale 1).
- Narrow implementation capture: `/home/DevCoordinator2/target/native-console-performance-1790601871362-6e37ff/performance-narrow.png` (390px CSS viewport, full-page 390×4705 capture, device scale 1).
- Combined comparison input: `/tmp/performance-design-qa-comparison-final.png` (source and final implementation crop side by side at 1487×1058 content scale; source 1487×1058, implementation top viewport normalized from 1440×1058).

The comparison was opened and inspected after capture. The selected matrix hierarchy is present in the live route: summary totals, API-equivalent estimate, activity cost table, outcome attribution, review history, partial coverage, and token detail popovers. The existing Performance destination and repository shell remain in place so the required outcome and review journeys keep their established navigation and labels; the selected direction is applied to the cost-matrix content inside that shell.

### Steps and findings

1. **Open repository Performance.** The hdlripper identity and Performance destination render in the first viewport. **Healthy.**
2. **Paint the page from the indexed snapshot.** The first `performance.overview` response returned in under 1 second and honestly showed a refreshing snapshot while the source read continued. **Healthy.**
3. **Complete the monetary refresh.** The background request returned 5,932,881,895 measured tokens and `$12,211.834247` API-equivalent USD in about 1.2 seconds, with Standard rate-card references and `partial` collector coverage. **Healthy.**
4. **Inspect the selected Review Cost Matrix.** Activity rows show provider tokens, API-equivalent USD, request counts, and coverage. Outcome rows retain token totals and per-outcome API-equivalent values. **Healthy.**
5. **Open a total-token detail popover.** The rendered popover exposes input, cached input, uncached input, cache write, output, reasoning output, and their USD components. **Healthy.**
6. **Check the narrow layout.** At 390px the matrix, popover, outcomes, and review history remain readable without horizontal document overflow. **Healthy.**

No actionable P0, P1, or P2 finding remains in the captured states. The source mockup uses illustrative model/file coverage values, while the implementation displays the repository's measured operations and truthful partial-coverage state. That content difference is an intentional data-state deviation, not a visual defect. The existing Performance title, review history, and outcome table remain intentional product constraints so the repository's established journey and attribution controls are preserved.

### Interaction and evidence

- Browser-rendered journey: Playwright against the live HTTPS Console; zero page errors.
- Checks passed: live commit identity, sub-second initial response, cost-enriched refresh, matrix visibility, USD value, all six token breakdown labels, wide screenshot, narrow screenshot, and zero narrow overflow.
- Live daemon source commit: `a5ab799fd1fd73e7d98a092f3ef2489515f83202`.
- Qualified delivery receipt: `delivery-63b896285d26903cdfb0428fc2104809245433ee5a23809eaaed2573d26acc08` from release `v7dafc4aafe8c3acb`.
- API-equivalent basis is an estimate using the administrator-managed Standard API rate-card catalog. It is not a subscription bill or provider invoice.
- Screenshot evidence does not establish full WCAG conformance; keyboard activation of the popover and rendered responsive geometry were checked, while assistive-technology output remains outside this pass.

### Fidelity surfaces

- **Typography:** existing Console typography and hierarchy are preserved; source and implementation use the same readable dark Console family and compact data labels.
- **Spacing and layout:** summary band, matrix table, outcome table, and review history preserve the route's established spacing; narrow reflow has no overflow.
- **Colors and tokens:** selected teal Performance state, semantic partial/complete badges, and API-equivalent USD emphasis are retained from Console tokens.
- **Assets:** the route uses the existing Console shell and no new image asset is required by the selected matrix direction.
- **Copy/content:** API-equivalent, Standard basis, partial coverage, and token component labels are explicit and truthful.

### Final checklist

- [x] Approved source target opened and inspected
- [x] Combined source/rendered comparison input created and inspected
- [x] Wide and narrow rendered captures saved and inspected
- [x] Cost refresh and token popover interactions exercised
- [x] No actionable P0/P1/P2 findings remain
- [x] Final result: passed

final result: passed

## Public upstream feature requests — 2026-09-30

Source: selected first option, Coordinator sketch `sd59f6dba5a44ad09`, and the
user-directed per-comment attachment refinement `s82efb78984dd8871` in batch
`kdddec2283058ce53`. The refined source SHA-256 is
`573148334d7d7a7474725c223076aba31b1d6e29df1a1d25236bcab8e99ff7f6`.
User decisions: `DC2-UPSTREAM-REQUEST-INBOX-SELECTED-20260929` and
`DC2-PUBLIC-FEATURE-REQUESTS-20260929`.

The selected inbox keeps the request collection beside the discussion. Both the
initial request and each comment retain their own files. Public readers can view
content while the originating server and upstream administrators use the same
thread for edits, replies, closure and reopening. Source mockups use illustrative
content; acceptance uses actual persisted tickets in isolated servers. Shared-Console
acceptance will use an explicit temporary ticket after installation. Existing Console tokens and
Inter remain authoritative, including the readable light variant.

### Reviewed journeys and evidence

1. Configure the upstream and retain existing ticket ownership: real Console
   save/reload and previous-upstream access pass.
2. Create, cancel and resume a draft, edit and remove the exact request: real
   persistence and downstream readback pass. A discovered immediate-edit/upload
   race was fixed by completing refreshed readback before closing the editor.
3. Reply with multiple files from both sides: attachment/message identity,
   retry without duplication, failure draft preservation, restart persistence,
   and paging through more than twenty replies pass.
4. Preview images, text and DOCX, and render multi-page PDFs: the native PDF
   iframe was replaced with local PDF.js rendering without document actions.
   Previous/next page, accessible document text and original download work.
5. Public read-only access and MCP parity: unrelated origins cannot mutate a
   ticket, public callers cannot select control-plane operations, and actual
   MCP clients on both servers create, discuss, attach files and close tickets.
6. Wide, phone and transition widths: tested at 1487, 741, 740 and 390 CSS pixels
   in light and dark themes. Primary content, controls and forms remain usable.

Isolated acceptance: `console/verify-tickets.mjs`, 13 passing journeys; evidence
`/tmp/dc2-ticket-journey-final/report.json`. Formal run
`formal-web-ui-mundh7yw-ff3b576c` checked eight discussion, creation and PDF cells,
with zero critical findings. The two media-surface warnings describe the white
PDF canvas and were resolved by screenshot review. Manual review receipt:
`/tmp/upstream-formal-pdf/manual-review.json`, eight pass decisions and zero gaps.
The exact selected source and rendered implementation were combined in
`/tmp/upstream-final-design-comparison.png` and inspected together.

Typography, copy, layout rhythm, token colors, hierarchy, responsive reflow,
attachment grouping, focus, cancellation and error recovery were reviewed. The
real attachment thumbnails were inspected after their bytes finished loading;
redacted Formal UI input captures do not imply missing control labels. The
source does not specify a phone layout, error state or full document viewer;
those states were checked against the agreed journey rather than a fabricated
source image. Full assistive-technology conformance remains outside this review.

Shared Console comparison and final result are recorded below after native
installation; the isolated result alone is not a shared-surface handoff.

### Shared surface and final status

The user authorized merge and installation on 2026-09-30. PR16 was merged and
installed through the reviewed native workflow. Shared acceptance then found a
phone action-menu label defect: the toolbar label-collapse selector also hid
text-only menu items. The direct-child selector repair is committed as
`c46ddc9`, installed, and verified by fourteen two-server journeys, including
phone/tablet close and reopen in both themes. This repair restores the selected
behavior and introduces no new design direction.

The shared Console comparison at `https://console.vr.ae/#/requests` uses the
actual installed implementation, a temporary public acceptance ticket, 1487px
and 390px viewports, dark theme, authenticated local administrator, and public
reader states. The mockup has illustrative ticket content; the rendered capture
has real persisted acceptance content and an additional initial-request file
strip required by the original request. Both images were combined in
`/tmp/upstream-shared-audit/source-render-comparison.png` and inspected.
The paired layout preserves the selected inbox/discussion hierarchy, local Inter
and semantic palette, readable text and compact controls. The phone layout
preserves the thread with a working return-to-list action. Real image thumbnails
and document links remain attached to their originating comment.

Shared acceptance passed eight checks: upstream ownership, both sides' persisted
replies, comment document viewing, narrow layout, closure, reload persistence,
public read-only behavior, and removal of the exact temporary ticket. The public
page and API returned HTTP200 through both `vr.ae` and `console.vr.ae`.
The installed MCP server reports the default upstream `https://vr.ae`.
The native evidence pass checked 1,150 observations, including every served
Console asset against the installed source. The running daemon source is
`c46ddc9c2e8decb89e686b89419ba715776e0874`.

Evidence: `/tmp/upstream-shared-audit/report.json`, paired capture above,
`shared-wide.png`, `shared-narrow.png`, `shared-public.png`,
`/tmp/dc2-ticket-phone-repair/report.json`, and native run
`native-tickets-1790766784122`. A final qualified delivery artifact retains these
reports and captures; its receipt is recorded in the authoritative release.
Earlier failed runs remain failure evidence and do not supply completion claims.

No actionable P0-P2 visual, responsive or interaction finding remains. The PDF
media-surface warnings are intentional document content, reviewed visually.
P3: real server-name avatars use the existing user icon instead of generated
initials. English-source fallback for ticket-specific copy is explicitly
recorded in the feature catalog; it is not claimed as translated coverage.

**Final result: passed.** Shared-source comparison and rendered interactions
are complete; the temporary acceptance ticket was removed after verification.
# Mockup history and continuation-set surface

final result: passed

## Selected source and implementation identity

- The user selected revised option 1 (“First option looks great!”). The source
  is the immutable Coordinator image bound to
  `DC2-SKETCH-HISTORY-SURFACE-REVISED-OPTION1-20261002`.
- Source image: `/var/tmp/dc2-mockup-history-sources/revised-1.png`;
  SHA-256 `383a9bd5d8881c3c841ff9512a4650afd7648eb605470de416a381765286a226`;
  1487 × 1058 pixels; one dark-theme desktop Console surface at the declared
  1440 × 1024 viewport.
- The implementation was captured from the rendered Sketches route in the
  fresh formal candidate. Representative evidence is
  `/tmp/dc2-sketch-integrated/formal-dark/screenshots/cell-0012-story-dark-wide-viewport.png`
  (1487 × 1058 CSS pixels at device scale 1), with matching light, desktop,
  intermediate, and phone captures in the same run.
- The live daemon reports source commit
  `ce6c717b8d545d7cda7f47df103addd92264b131`; the shared Console source and
  static assets were checked against the installed canonical tree. The exact
  Coordinator records resolve to one current node, six graph nodes, and three
  `adjusted_from` edges.

## Comparison and interaction evidence

- Full-view comparison checked the collapsed current-version summary, one
  selected surface scope, the visible batch option strip, current status,
  story context, and the branch-aware history column. The source and render
  use the same layout and state; the fixture's diagram text is intentionally
  different test content, so no invented product content was treated as a
  visual defect.
- Focused comparisons checked the current-version disclosure, multi-option
  continuation cards, rejected/undecided statuses, initial agent context,
  latest comments, annotations, and the history timeline. No additional
  focused crop was needed for typography or controls because those regions are
  legible in the wide and narrow captures.
- The formal browser receipt passed 32 cells in each theme, including gallery,
  bounded search, story, scope, context, comments, editor, and empty-search
  states across phone, intermediate, desktop, and wide sizes. The manual
  receipt passed every corresponding cell. The real API journey passed 36
  checks, including publish validation, duplicate digest and cycle rejection,
  multi-selection persistence, legacy non-promotion, description revisions,
  paging, restart persistence, and stale-write prevention.
- Product Design audit findings: no actionable P0, P1, or P2 differences.
  Typography, spacing/layout rhythm, colors/tokens, image treatment, copy,
  focus, responsive reflow, and current/history semantics were reviewed in
  both themes. The only evidence limit is that screenshots cannot prove full
  assistive-technology conformance; keyboard and rendered interaction checks
  covered the declared journey controls.

## Comparison history

- Earlier evidence was intentionally marked blocked because it covered only
  the general Console and had no Sketches assertions. That record remains
  historical and was not reused as a pass.
- The fresh formal candidate added the complete Sketches matrix, the paired
  manual review, and the real publish → search → story → resolve → reload
  journey. It found no P0–P2 issue, so no visual fix or direction change was
  required after the selected source was confirmed.

## Final checklist

- One immutable image per surface/state/theme/viewport, with agent-authored
  searchable context and parent lineage: passed.
- All options, rejected nodes, multiple continuation selections, mutable
  comments, and legacy records remain inspectable: passed.
- Current implementation source comes only from explicit Coordinator
  resolution, never timestamps or stale Keep flags: passed.
- Formal receipt, manual receipt, Product Design comparison, live source
  identity, and real API persistence evidence are retained: passed.
# Storage Console design QA

Source visual truth: `Coordinator sketch image `s7b4de24cfd2223ad` (retained source image)` (selected Coordinator sketch `s7b4de24cfd2223ad`, 1487 × 1058, dark theme).

Implementation comparison: [/mnt/build-storage/dc2-storage-provider-candidates/product-audit-20261003/implementation-dark-wide-viewport.png](/mnt/build-storage/dc2-storage-provider-candidates/product-audit-20261003/implementation-dark-wide-viewport.png), 1487 × 1058 CSS pixels, device scale factor 1. The paired comparison is [/mnt/build-storage/dc2-storage-provider-candidates/product-audit-20261003/source-vs-implementation-dark-wide.png](/mnt/build-storage/dc2-storage-provider-candidates/product-audit-20261003/source-vs-implementation-dark-wide.png).

## Journey review

1. **Open Storage and scan the inventory** — passed. The collection, filters, Scan action, coverage notice, and measured artifact rows are visible. The live scan operation returned a durable job and the Console kept reads bounded.
2. **Filter and inspect safe data** — passed. Safe-to-delete rows show their concrete reason, size, ownership, last check, and scheduled cleanup. The inspector exposes dependencies and policy details.
3. **Protect and remove protection** — passed. Both actions persisted through the real `storage.protection.set` operation and kept the inspected artifact visible after its safety state changed.
4. **Plan and delete an exact safe target** — passed. A rebuildable `.ruff_cache` was selected, planned, deleted through the shared cleanup engine, and verified through receipt `sjeb9a3d8769eb54921f7c12bd` with 106,496 bytes reclaimed.
5. **Edit policy and review history** — passed. The policy form reads and writes the native 3-day/14-day defaults, and cleanup history reads durable receipts.

## Comparison findings

The implementation preserves the selected direction's hierarchy: a compact collection on the left, a persistent inspector on the right, green safe-to-delete status, locked current resources, concrete reasons, and an explicit permanent-data warning. The runtime uses observed inventory data, so the live comparison shows actual unknown and needs-review volumes rather than the retired GlobalFinance group from the concept image. That content difference is required for truthful behavior and does not change the selected layout or interaction model.

Typography, spacing, colors, icon treatment, responsive reflow, copy hierarchy, and the dark-theme balance were reviewed against the paired 1487 × 1058 captures. No actionable P0, P1, or P2 visual or accessibility finding remains. The formal browser run covered 15 cells across phone, intermediate, breakpoint, desktop, and wide layouts with `formal.result: passed`; the manual screenshot review covers all 15 cells in `/mnt/build-storage/dc2-storage-provider-candidates/console-formal-2a0d7ae/manual-review.json`.

P3 follow-up: the row checkboxes are visually compact. The table keeps their programmatic names and the surrounding row remains usable; increasing their visual hit area can be considered in later polish.

Comparison history: the initial implementation was revised to keep the inspector visible after protection changes, remove narrow-screen table overflow, shorten only the filter-menu labels that could not fit native controls, and stabilize the initial inventory render. The final paired comparison above was captured after those fixes and found no actionable P0-P2 issue.

**final result: passed**

## Agent Settings — Policy Matrix (2026-10-04)

### Source and implementation

- Source visual: Coordinator sketch batch `kd521ccda0a112e60`, selected node `sa69a7f252db8c65d`
- Source identity: SHA-256 `6d4dbfd15b2b23f5bc97686f5300d916a00821150e402f75c63cf518ff586706`
- Implementation capture: `design-qa/agent-settings/implementation-dark-1487x1058.png` (SHA-256 `507e62074443d3721d2a703d6e0bff8bfd01a4fc6877098278108664c5b88993`)
- Comparison conditions: dark theme, populated administrator state, 1487×1058 CSS viewport, one Console surface. The implementation capture was inspected after saving and the source was inspected from the selected immutable sketch.

### Findings

- The implementation preserves the selected matrix hierarchy, dark Console token family, compact scope/harness/capability strip, role table, and bottom save action.
- The current capture uses three fixture roles while the approved source depicts the full 13-role product state. This is a comparison evidence limit; the live candidate’s seeded role registry contains the complete 13-role set and must be captured again in the final handoff.
- No actionable P0/P1/P2 visual issue was found in the captured state. The final comparison remains preliminary because the live Coordinator delivery and source-identity receipts were unavailable.

### Verification limits

- The screenshot proves rendered hierarchy and visual state only; it does not prove assistive-technology behavior, live persistence, authorization, or the complete 13-role populated surface.
- The repository’s Coordinator service timed out while final delivery evidence was being requested.

final result: blocked

## Usage selection visibility and completeness chrome repair (2026-10-08)

### Source and implementation

- Approved source direction: Coordinator decision `DC2-USAGE-COST-LEDGER-REDESIGN-20261001` (`n24658c01816d4dd4`), retained Cost Ledger mockup asset `01a0df7a-c47a-78d0-bc42-fa890c1c8c6e/exec-698e1bd7-6703-4ce9-8824-eb2f8c495d9b.png`, SHA-256 `a9569621187fa9ec6715a4225546439d392d8789db3762213aae66a6e977ef24`.
- Implementation source: worktree commit `bfa9f70b2109de959a7bb24fb51e380b4c458ffe`.
- Comparison conditions: dark Console theme, repository Usage route, populated usage fixture, 1487×1058 CSS viewport, authenticated operator state, same source target and route family.
- Implementation captures: `/tmp/dc2-usage-selection-audit/01-base-dark-1487x1058.png` (SHA-256 `98c36431259d1b14f1a5dba0827b3ee23914f9abebf44e20e7a3d55470f1b323`) and `/tmp/dc2-usage-selection-audit/02-model-selected-dark-1487x1058.png` (SHA-256 `d9f25019e3b7d6c5e683fd8baaf3f0a63759a0d48671454191dc421451657a5e`).
- Formal receipt: `formal-web-ui-muzvtubl-669ea5d3`, `formal.result: passed`, candidate `c28a36b5880e41281c0a92662e9981510b80ba7ed43ae3b0c38352387f21f433`, source SHA-256 `1ec21c41725d9dacdaaa95b698b4634c739b773ca0505c56c1d43d4cb033e232`, artifact manifest SHA-256 `30d96352567fc9fc3d9bc501a7a607544d5290236c805eddde398d7c70323363`.
- Manual receipt: `/tmp/formal-web-ui-verification-HZXbsq/manual-review.json`, `manual.result: passed`, all four cells reviewed.

### Journey review

1. **Open repository Usage** — passed. The usage title, range controls, token summary, phase chart, model ledger, activity and timing sections are readable in the initial and full-page captures.
2. **Select a model** — passed. The selected row remains recognizable and the detail panel appears in the same viewport after activation on desktop and mobile.
3. **Read incomplete-data state** — passed. The large repeated completeness sentence and bottom provenance block are removed; a compact state indicator and labelled information hint preserve access to the explanation without displacing the dashboard.
4. **Use the phone layout** — passed. The title occupies its own row, range controls and coverage hint share the following row, and no content overlaps or clips.

### Findings and iterations

- Initial formal candidate was blocked by a loading capture, missing fixture-shape/source bindings, and a zero-width model-name track. Those were corrected before the passing run.
- The first visual pass found the model names unreadable in the narrow ledger column. The row now gives the model name the flexible track and collapses redundant bar/share columns while retaining cost, status and chevron affordances.
- The first phone capture found the range switcher overlapping the Usage title. The repository-scoped header now uses a two-row grid below 760px.
- Typography, spacing, colors/tokens, icon treatment, responsive reflow, copy, and interaction continuation were reviewed in the paired captures. No actionable P0, P1, or P2 issue remains.

### Accepted deviations and limits

- The approved source includes the older visible completeness sentence and a cache-rebuild state. The current user-directed repair intentionally removes the duplicated sentence/provenance panel and retains the honest state through the compact indicator and on-demand hint; the rebuild state was not active in this populated fixture.
- The source and implementation use different measured values and model names because the implementation is bound to the real usage fixture; this is an expected content difference, not a visual-target substitution.
- Screenshots do not prove persistence, authorization, or assistive-technology behavior; those are covered by the rendered interaction suite and remain distinct from this visual comparison.

**final result: passed**
