# Console (Phase 7)

Static browser application in `console/` (no build step, no dependencies),
served by the edge on the console host to signed-in users, driven only by
the edge's `/api/v2/<operation>` bridge. Every visible enabled control calls the
real API and re-reads state afterwards; nothing is a placeholder, nothing
fakes success, and no view carries fixture numbers.

## Navigation shell

One shared workspace owns repository selection across the Console. Its left
sidebar provides search and a checkout disclosure. Drag its right edge or focus
the separator and use arrow keys to resize it; Home/End select the limits and
Enter hides it. The header button restores the list. Width and visibility are
remembered in this browser. Ordinary names stay on one line, with full names in
tooltips when a long name needs truncation. Below 761 px, the same list
opens as a keyboard-accessible drawer without displacing the requested content.
Repository aspects are ordinary hash links: **Plan & progress**, **Deployments**,
**Tests**, **Decisions**, and **Glossary**. Plan, Progress, and Usage are views
within Plan & progress, not independent repository pickers. On phones, aspect
links wrap so the selected destination remains visible without sideways scrolling.
Multiline checkout paths retain their own row height in long repository lists.
Each repository shows its root beneath the name and in the selected header.
The Plan repository index supplies roots and verified Git source identity even
when no test results exist or the Tests request fails. Matching source clones
share a name and keep their exact checkout links; folder names and nesting alone
never combine repositories. Unavailable ancestry retains the registered root.
Checkout context below the root uses a relative path when it is inside that root;
the checkout disclosure retains every full path and exact record link.
Plan and Decisions reuse the shared repository selection without rereading the
index for a hidden picker. Tests load when opened, or as a fallback if the Plan
index is unavailable, so ordinary repository navigation does not wait for them.
Administrators can use the pencil beside the selected repository name to choose
a Console-only name and icon. Save persists these in repository presentation
metadata; Cancel leaves the saved appearance unchanged, and Use defaults restores
the original name and folder icon on Save. Repository IDs, Git names, checkout
paths, record ownership, and access grants do not change.
Repository selection
is encoded in the URL and remembered for unscoped legacy links; Back, forward,
and reload preserve the exact context. Deployment and screenshot deep links
select their owning repository. Unknown explicit identities never silently
switch to another repository.

The header's Console menu holds host-wide Health, Bugs, Shared glossary,
Test capacity, Log retention, and authorized Administration. These tools do not
pretend to be scoped to the current repository. Returning to Repositories
restores the remembered selection. Menus close on outside click, activation,
or Escape and restore focus after keyboard dismissal.

The shared catalogue combines authorized planning, usage, progress, deployment,
and test indexes. Only verified origin keys combine separate checkout records.
The checkout disclosure preserves access to separate plans and decision records;
test and deployment actions always use their original exact identifiers.
This is presentation grouping, not a database ownership merge
(DC2-2026-09-07-SHARED-REPOSITORY-WORKSPACE).

## Shared visual language

The signed-in Console uses locally hosted Inter and semantic color tokens in
`design-system.css` across collection, detail, chart, dialog and inspector
templates. The initial light/dark mode follows the browser preference; the
header switch persists an explicit choice across navigation and reloads.
Authentication and upstream-error pages use matching colors with a system-font
fallback, keeping those documents self-contained before sign-in.

Unversioned assets revalidate using ETags rather than remaining immutable for an
hour. The document's versioned asset requests also bypass caches populated by
the earlier caching policy, so a normal refresh can load the redesigned code.

## Destinations

1. **Deployments** — the selected repository's deployments appear directly,
   with state, domain, port, generation, and recency. Cross-aspect summaries and
   another repository list do not precede them. Deployment facts adapt to the
   remaining workspace width and stack on phones without document overflow. An
   **edit** button beside every administrator-visible domain opens the pop-up
   domain editor in place; start/stop/restart remain available to operators —
   observed ones drive the exact recorded containers
   (DC2-2026-08-24-OBSERVED-LIFECYCLE) — apply for administrators on
   managed ones; detail page with components (state, health, generation,
   port, restarts, exact binding, last error), per-component controls and
   nested Compose-service state; only explicitly independent long-running
   services receive exact start/stop/restart controls,
   on-demand logs (managed files or observed `docker logs`),
   rollback/remove (managed, administrators; separate immediate controls keep
   persistent data or delete it), the same pop-up domain editor
   (administrators; `deployment.set_domain` — for an observed deployment
   without a route it asks for the host port, and a re-import replaces
   observed edits), and per-component CPU/memory charts over a selectable
   1h/24h/7d/30d window. The detail page names the repository under the
   heading. While a deployment is applying, conflicting mutations are disabled
   and a notice explains that closing the page does not cancel the accepted
   operation; the caller refreshes status after it finishes.
2. **Plan** — the completion ledger as an interactive Gantt workspace for the
   selected repository, with its current release, done-lines progress,
   open-task count, and preview-requested state.
   A sticky, collapsible and resizable task navigator keeps the arbitrary-depth
   tree aligned with a dominant, independently scrollable cumulative-lines
   canvas. The canvas supports pointer and keyboard pan, zoom, fit, a draggable
   minimap, animated hover details, synchronized task selection, direct
   drag-and-drop, and truthful right-edge estimate resizing with an exact-number
   dialog as the keyboard/touch path. A distinct **Not estimated** band gives
   every unknown-size leaf and parent summary a selectable non-proportional
   mark without adding invented lines to the numeric axis; its selected tray
   offers **Add estimate**. Parent tasks are summary spans; releases
   group in sequence with boundaries and per-release progress; delivered releases link to the running app
   ("Open the app ↗") or name the server port. Everything is plain
   language: statuses read planned / being built / done / delivered, owner
   feedback carries a "your request" badge, long labels reveal their complete
   text through selection/hover, and the agent-facing `technical_note` is never
   rendered. A compact bottom tray keeps the selected task, progress, impact,
   release, move, resize and drop actions in context; narrow screens use a
   compact summary and focused bottom sheet. Selection, task disclosure,
   navigator disclosure, and tray disclosure update the existing workspace
   locally, preserving scroll/focus without another plan read or whole-page
   replacement. Owner controls (administrators):
   drag-and-drop a task between releases or to a new position within one
   (drop between rows or onto a release header; delivered releases refuse
   drops), the move pop-up as the touch/accessibility path, "Request
   preview now" (replaced by a pending notice while one is requested),
   explicitly labelled immediate drop-task action, and an "Ask for a change" form that files a
   `user_feedback` task. Viewers get the same chart read-only.
   Collector-backed displays use daemon-memory snapshots shared by repository
   and exact window. Cold reads show loading; stale reads retain saved values
   while a background refresh runs. Snapshot time and refresh failures remain
   visible. Completion waits update the page without polling and preserve open
   counting details and range focus. Restart clears this disposable cache; no
   collector database is copied or changed. Source indexing remains necessary
   when a collector cannot be read within its existing deadline.
3. **Progress** — an operator/administrator repository dashboard that aligns
   terminal task completions, current planned task lines completed, terminal
   test pass rate, and provider total-token use on hourly, daily, or
   Monday-aligned weekly UTC buckets. Tasks and planned lines use bars for each
   bucket and a separate thin running-total line; the legend says exactly what
   each mark means. The line paints behind bars and haloed value labels so
   crossing geometry never hides a number. Open release work follows the Plan order and shows the
   owner-facing title, recorded status, estimate, elaboration request, and a
   simple reopened state without exposing raw planning/event notes or inventing
   priority, dependency, or impact days. Selection is local; **Open selected in
   plan** continues to the exact task. The release forecast is a provisional date range paired with
   confidence and a Forecast quality explanation naming missing estimates and
   the missing target date. Test and token evidence explain their own gaps so
   token coverage is not misrepresented as a forecast input. The forecast
   becomes unavailable when there is no release or measurable completion pace.
   Missing test/token history stays blank rather than becoming zero. Exact
   bucket values and counting semantics remain available without hover.
   The complete legend stays in the visible card while the chart pans. Hover,
   focus, or tap a bucket to see its UTC period and values, including completed
   and added work and the completed running total. Arrow keys move between
   buckets; Home/End select the first/last, and Escape dismisses the values.
   Test and token points expose only observed data; missing points stay blank.
4. **Decisions** — the per-repository decision history in plain language:
   "The story so far" (latest rolling summary, expandable in place), a full-text search box over
   every decision ever recorded, an aspect filter (server-side), entries
   newest-first with aspect badge, optional stable ref, and age; superseded
   decisions collapse and dim; "Show older decisions" pages the permanent
   history.
5. **Tests** — the shared sidebar retains repository selection. The
   selected repository's results appear immediately, newest first,
   without repeating its name. Matching verified Git origins group independent
   release clones under the source repository name, including bounded,
   caller-readable chains of local Git origins and linked worktrees. Missing
   ancestors, cycles, and ordinary subdirectories never infer a parent. Every action retains
   the exact run and checkout. Repositories without a usable origin keep their
   registered identity; equal display names alone never merge repositories.
   Selection persists across reloads. Each result shows its test, status, date,
   duration, direct actions and small screenshot thumbnails. Check results,
   validation tier, worktree path, exit code and output sizes stay in **Details**.
   While any listed run is active, refresh preserves open details, the inline
   run form, dialogs and scroll position and stops after navigation.
   **Logs** opens a focused catalogue-first dialog and
   immediately shows the selected stream's newest bounded text. Scrolling to
   the earlier boundary loads one prior page without moving the line being
   read; short pages fill only until they become scrollable. Search and likely
   failure results extend at their lower boundary, while **Jump/Refresh latest**
   remains explicit. Numbers and common syntax/status tokens are highlighted;
   valid JSON and JSON-lines are pretty-printed, textually labelled, and still
   escaped and labelled untrusted.
   The Console menu contains **Log retention**, which edits the host age/depth
   boundaries and re-reads the stored state. **Run tests** reveals an inline
   form for the selected repository with release selected by default.
   **Run again** directly repeats the exact checkout and recorded tier; a
   running row instead offers **Stop run**. The menu also contains **Test capacity**, which opens a
   focused dialog showing learned/effective capacity, the optional maximum,
   active/waiting leaves, admission pause state, and the last adjustment's
   measured evidence. Saving or clearing the host-wide maximum acts directly.
   Screenshot previews open a gallery with previous/next buttons, arrow-key and
   Home/End navigation, a current-image count, and a horizontally scrollable
   thumbnail strip. All available viewport and full-page captures are included;
   image bytes load on demand. Native retained images use the same gallery.
   The active image's viewer/comment or file action retains its exact run and
   image identity. Earlier-run provenance stays visible; Escape restores the
   originating thumbnail or more-images button even after a background refresh.
   **Files** opens declared retained files from the exact selected run and
   check, including native screenshots and nonvisual reports. **Run** also lists
   earlier runs from the bounded `test.history` record; choosing
   one discovers its own check catalogue and file collections, never inheriting the latest test
   name, status or file list. A newer managed or nonvisual run therefore does not
   hide still-retained native screenshots. The focused viewer
   pages the file catalogue with its manifest identity and previews raster images.
   File labels omit generated identifiers and distinguish repeated readable names.
   XML, JSON and test reports present meaningful fields and expandable sections with
   highlighted keys, values and outcomes, not markup, namespaces or opaque hashes.
   Numeric text stays exact; nanometre measurements display losslessly in millimetres.
   Malformed, unsupported or incomplete structured data has an explicit unavailable
   preview rather than a raw-markup fallback. Other text keeps escaped highlighting.
   Downloads preserve the original bytes through verified bounded chunks. Text previews stop at 1 MiB; files above the 32 MiB browser
   limit remain available through the retained-file command line tools.
   Closing or changing selection discards stale reads and releases image data;
   expired, denied, changed and unavailable files retain an exact retry.
   Formal journey screenshots and retained native images appear as small
   thumbnails beside their result without opening another disclosure. A click
   enlarges the image; the preview opens that exact formal screenshot in the
   existing viewer/commenter, or the exact native image in the retained-file
   viewer. A compact remaining count opens the full formal collection. Images
   load only near the visible results, through bounded, identity-checked reads.
   Nonvisual runs do not show redundant screenshot-unavailable labels. Read
   failures remain beside the result with a retry, never a broken-image success.
   The latest retained earlier formal run can supply previews with its actual
   date and an explicit **Earlier run** label. Opening it resolves that exact
   run in the same worktree, never pretending its screenshots verify the current
   run. A retained formal UI bundle opens as the
   selected three-zone review board: ordered journey states and viewport
   choices on the left, the immutable screenshot and complete annotation
   toolbar as the dominant centre workspace, and capture facts, automatic
   finding kinds, and discussion on the right. Viewport/full-page and
   desktop/mobile changes are local. On narrow screens the journey becomes a
   horizontal step picker and the inspector becomes a focused bottom sheet.
   Select, pin, rectangle, arrow, freehand, highlight, text, colour,
   undo/redo, zoom, fit, pan, and clear are functional. Saving a marked
   suggestion starts with an immediately focused, nonmodal comment editor after
   a pin or completed drawing. Another pin click repositions the current unsaved
   pin, preserving its comment. Text labels have explicit Add and Cancel actions
   and accept short labels such as “X”. Selecting a draft mark allows moving,
   recoloring, deleting, and undoing changes. Cancelled pointer gestures add no
   mark. Switching journey steps or screenshot variants preserves each image's
   unsaved comment and marks for the current page session; they are not saved
   across a browser reload. Failed saves preserve the draft for retry. Journey
   and Details controls independently hide and restore the side panels; normal
   panel choices persist in the browser. Full screen initially hides both panels
   and keeps the same interactive canvas, image navigation and comment editor.
   Panels can be reopened there without changing the normal layout. The exit
   control or Escape restores that layout without discarding the draft. Browsers
   without native fullscreen use the full browser viewport. Saving a marked
   suggestion atomically creates a Plan `user_feedback` task; replies, author
   edits, resolve/reopen, and explicitly labelled author deletion remain linked
   to the exact screenshot. Retained screenshot links remain usable after newer
   runs replace the recent-results list; links retain their worktree identity and
   ambiguous copied captures never silently select a different repository.
   Missing, expired, invalid, tampered, or unauthorized
   evidence is stated honestly and never replaced with a mock image.
6. **Health** — host condition first as one aligned capacity group (CPU,
   memory, root filesystem, load/swap) beside a separate operational-status
   group (unhealthy deployments, critical alerts, active tests, total
   containers, and class counts). At 1200 px and below the two groups stack;
   incident cards keep their natural height. **Unhealthy deployments** names
   exactly which component is unhealthy and why (`reasons` from
   `health.summary`) with start/stop/restart and a link to details and
   logs; current alerts; **History** — host CPU, memory, and storage charts
   (min–max band plus average) over a selectable 24h/7d/30d window using
   server-side downsampling; structured CPU and memory reconciliation; then
   one row per
   repository with CPU/memory/storage/health/trends plus the DevCoordinator
   and shared/unattributed rows. Shared storage is a set of complete labelled
   values instead of one clipped sentence. At 960 px and below repository rows
   become labelled cards without document-level horizontal scrolling.
   **Containers** view: every
   container with full identity, state, classification, repository,
   deployment/test, caller and client, CPU/memory/layer size, creation
   time, TTL; removal is offered only for orphaned-managed and managed-test
   containers (unmanaged ones say "decide manually").
7. **Codex Usage** — an operator/administrator view within Plan & progress,
   scoped to the selected repository, with combined 24h totals and a
   plain-language **Data included** status. A neutral
   status means setup is not connected everywhere; amber means measured data is
   partial, green means complete, and red is reserved for a real read failure.
   Repository details lead with one
   continuous total/request/tool/execution strip and a stacked UTC token chart
   whose only additive basis is provider `total_tokens`, grouped by work phase.
   Ranked activities, separate request-wall/execution-union/summed-agent rails,
   tool outcomes, exact bucket values, and data-completeness details follow.
   The interface calls each configured input a **Codex environment**. The
   consequence-first status remains visible; a small adjacent information hint
   opens on request to explain that each environment is a separate local Codex
   setup with its own usage history. When an environment supplies no data or a
   measurement is missing, the status says some usage may be missing and the
   hint explains that absent values are excluded rather than counted as zero.
   Internal collector status remains an API detail. The page returns no contributor identities, private
   paths, raw Codex IDs, captured content, or per-user values.
8. **Bugs** — open records with occurrence counts and correlations; report
   form; close.
9. **Administration** (administrators only) — users and grants, invitations
   (invite form with optional initial grant), Telegram chats and
   subscriptions (link code, subscribe), server versions and the served
   route-document generation.

Non-administrators see only the destinations and data their grants allow;
server-wide health and the Containers/Tests/Administration views render an
explicit permission-denied notice instead of partial data.

## Interaction inventory

| Control | API call | Proof of state change |
|---|---|---|
| Shared repository selection | Authorized Plan, Tests, Deployments, Progress, and Usage indexes; then real hash links and selected-detail reads | One searchable list preserves repository context across aspects, Back, and reload. Verified checkout groups retain exact run and record identities; unrelated matching names remain separate. |
| Repository drawer and Checkouts | — (client-side) | The narrow-screen drawer focuses search, traps focus while open, and restores focus on Escape or cancellation. Checkouts reveals verified paths and explicit links to separate records. |
| Deployment start/stop/restart (list, detail, component; managed and observed) | `deployment.start/stop/restart` | view re-fetches `deployment.status`; header/component badges change |
| Independent Compose-service start/stop/restart (detail only; explicitly declared services) | `deployment.start/stop/restart {component: "stack/service"}` | service badge and aggregate header change; unrelated service and route remain |
| Domain edit / clear (pop-up from list rows and the detail page, administrators) | `deployment.set_domain {deployment_id, domain|null, port?, public?}` | status re-read; route document republished |
| Health range switch (24h/7d/30d) and usage range (1h/24h/7d/30d) | `health.history {minutes, points}` | charts re-render from the store |
| Health container inventory / unhealthy deployment details | — (real hash links) | opens the Containers or exact deployment destination; Back/Health returns to the same Health context |
| Codex Usage repository selection and range (24h/7d/30d) | `usage.repositories {range}` / `usage.repository {repository_id, range}` | repository heading, totals, phase chart, exact table, and data-completeness explanation re-render from canonical reads |
| Codex Usage completeness hint | — (client-side) | opens the full environment and excluded-not-zero explanation in a labelled DOM pop-up; Escape, focus departure, outside click, or the toggle closes it |
| Progress period (Hour/Day/Week) | `progress.repository {repository_id, period}` | completed bars above the baseline and newly added work below it share one scale within each lane, so equal values have equal lengths; a protected label area keeps titles clear of first-bucket maxima; completion running totals, test/token evidence, forecast quality, Plan-ordered work, comparison totals, and exact values re-render from one bounded report |
| Select release work | — (client-side) | only row selection and the Plan-continuation target change; the workspace node, scroll, focus, task order, and release scope remain unchanged |
| Open selected in plan | — (real hash navigation with local task continuation) | opens the same repository Plan with the exact selected task highlighted |
| Progress exact values disclosure | — (client-side) | exposes every visible bucket value, coverage status, and counting method without hover |
| Aspect and work-view links | — (real hash links followed by selected-detail reads) | Changes the current aspect or Plan/Progress/Usage view without losing the repository; legacy destination links resolve to the remembered selection. |
| Console tools menu | — (client-side) | Opens host-level destinations and test settings; Escape closes the menu and restores focus. Settings dialogs return focus to the menu trigger after saving or cancelling. |
| Unhealthy-deployment actions (health page cards) | `deployment.start/stop/restart` | summary re-read |
| Apply / rollback | `deployment.apply` / `deployment.rollback` | status re-read |
| Remove deployment — keep data / Remove deployment and delete data | `deployment.remove {delete_data: false|true}` | list re-read |
| Component logs | `deployment.logs` | tail rendered on demand |
| Test logs | `test.log.catalog`, then automatic bounded `tail`; scroll boundaries, Jump/Refresh latest, Search, and Show likely failure compose `tail`, `search`, and `failure_context` | newest text is immediately readable; earlier/results cursors load through scrolling without a paging button; position survives prepends; JSON/JSONL is formatted and escaped; useful tokens are highlighted; technical details stay collapsed |
| Test start/stop | `test.start {tier}` / `test.stop` | list re-read; the selected tier is recorded |
| Test capacity | `test.capacity.get` / `test.capacity.set {cap: integer|null}` | dialog and Tests action re-read learned/effective capacity and the administrator maximum |
| Test log retention | `test.log.retention.get` / `test.log.retention.set {max_age_seconds, case_depth}` | focused dialog re-reads the stored age/depth boundaries; active logs remain protected |
| Open visual journey evidence | `test.evidence.get {path, run_id}` followed by bounded `test.evidence.image` chunks for the selected image only | opens the exact run, orders declared route/state/viewport cells, verifies and renders the immutable screenshot without exposing a path |
| Select journey step, viewport, or viewport/full-page capture | — (client-side) | only the review board selection, thumbnails, capture facts, findings, overlays, and discussion change; the Tests collection is not re-read |
| Draw screenshot feedback | — (client-side draft) | select/move/resize/delete draft, pin, rectangle, arrow, freehand, highlight, text, colour, undo/redo, zoom, fit, Space-pan, and clear update the canvas truthfully; Clear affects unsaved marks only |
| Create screenshot feedback | `test.evidence.feedback.create {path, run_id, image_id, body, marks}` | creates the visible thread and an exact repository Plan `user_feedback` task in one transaction |
| Reply/edit/resolve/reopen/delete screenshot feedback | `test.evidence.feedback.reply/edit/state/delete` | the thread re-renders; root edits update Plan wording, resolve/reopen updates task state, and explicit author deletion drops the task while retaining history |
| Container remove (orphaned/test only) | `health.container_remove {container_id}` | inventory re-read |
| Bug report / close | `bug.report` / `bug.close` | list re-read |
| Invite, remove user, set/remove grant | `user.invite`, `user.remove`, `grant.set`, `grant.remove` | administration re-read |
| Telegram link / subscribe / unsubscribe | `telegram.link`, `telegram.subscribe`, `telegram.unsubscribe` | administration re-read |
| Drag a task between rows or onto a release header (Gantt) | `task.update {task_id, position, release_id?, parent_task_id?}` | plan re-read; the bar moves |
| Move/reorder pop-up on a task row | `task.update {task_id, release_id|null, position?}` | plan re-read; the bar moves under the chosen release |
| Resize a leaf task from its selected bar or exact-number dialog | `task.update {task_id, estimated_loc}` | plan re-read; the estimate persists and following cumulative bars reflow |
| Add the first estimate to an unknown-size leaf | `task.update {task_id, estimated_loc}` | the dashed non-proportional mark becomes a proportional bar after the plan re-read |
| Elaborate on any task row or selected-task tray (administrator) | `task.update {task_id, elaboration_needed: true}` | the existing row and tray are marked `elaboration needed` without replacing the Plan; a persistent notice explains that agents receive the request. The action remains visible but disabled as `Requested` until an agent saves clearer title/outcome wording and clears it atomically. Viewers see the mark but no request control. |
| Select a row/bar and hover a bar | — (client-side) | row and bar highlight together; the bottom tray and anchored hover badge reveal the same task |
| Pan, zoom, fit, horizontal scroll, and minimap navigation | — (client-side) | the cumulative-lines viewport and minimap stay synchronized |
| Select, collapse/expand a task, collapse the selected tray, or collapse the navigator | — (client-side) | only affected DOM state changes; no `plan.overview`, loading state, whole-workspace replacement, or page flash |
| Request preview now | `release.request {repository_id}` | plan re-read; pending notice replaces the button; delivery later shows the app link/port |
| Ask-for-a-change form | `task.create {repository_id, title, impact?, kind: user_feedback}` | plan re-read; the task appears with a "your request" badge |
| Drop task | `task.update {task_id, status: dropped}` | plan re-read; the task leaves the chart (history kept) |
| Collapse/expand a parent task | — (client-side) | subtree rows hide/show |
| Decision aspect filter / Show older | `decision.tail {repository_id, aspect?, n, before_seq?}` | list re-fetched server-side |
| Decision search | `decision.search {repository_id, query, aspect?}` | matching decisions rendered |
| Sign out | `/auth/logout` | session cleared |

## Browser verification

`CONSOLE_VERIFY_PLAYWRIGHT=<dir with node_modules/playwright> node
console/verify.mjs` runs the real edge and Console against a fake daemon
with fixture scenarios — populated (long names, large numbers, degraded,
alerts), empty, partial analytics, error, loading, permission-denied — at 1280×800 and
390×844 for every destination, checking: no horizontal document overflow,
no clipped headline text, no off-canvas controls outside scroll containers,
explicit empty/error/loading/denied states, humanized large numbers; and
clicks through stop/start/logs/remove/test catalogue and bounded retrieval/bug report/invite/
container removal proving each calls the API with the expected arguments
— including visual-evidence metadata-before-image loading, every annotation
tool, local step/viewport/capture switching, feedback creation into Plan,
reply/edit/resolve/reopen/delete, immutable-image behaviour, and the narrow
feedback bottom sheet —
and re-renders — including Progress period/release-work/exact-value/Plan
continuation, the Plan drag-and-drop (reorder and cross-release),
move pop-up, preview request, feedback form, task drop, decision
filter/search/paging, and a plain-language proof that the agent-facing
technical note never renders. Screenshots and `report.json` are written to
`CONSOLE_VERIFY_OUT` (not committed). The Administration Server line is
asserted to render the daemon version, schema, and served route-document
generation (the edge accepts the dotless `ping` alongside dot-separated
operation commands). The Plan interaction pass also proves select, pan, zoom, fit,
minimap, navigator collapse/resize, hover, pointer resize success/cancel/failure,
exact resize, persistence after reload, modal cancellation, unknown-size marks,
first-estimate persistence, 111 simultaneous unknown verification jobs,
zero-refetch/zero-replacement selection and disclosure, and read-only or
delivered-release protections. The Codex Usage interaction pass additionally
proves linked navigation, repository selection, phase stacks, all time ranges,
focus restoration, separate timing rails, plain complete/missing/empty/unavailable
data explanations, and exact non-hover values and large-scale axis-label separation.
`CONSOLE_VERIFY_WORKSPACE_ONLY=1` checks shared repository selection, all aspect
links, Plan/Progress/Usage context, Back and reload, domains, search, drawer
focus, and host-tool separation at 1280, 713, and 390 px in both themes.
`CONSOLE_VERIFY_TESTS_DESIGN_ONLY=1` checks the scoped test results, gallery,
earlier-run provenance, exact viewer continuation, and test actions.
`CONSOLE_VERIFY_ARTIFACTS_ONLY=1` checks retained native files and previews.
These are focused development passes; final validation must reference its own
exact source candidate and retained report, not an earlier pass count.

The Health layout has an additional focused journey gate at 390×844,
856×915, 1440×1024, 959/960/961×915, and 1199/1200/1201×915. It verifies
capacity/status hierarchy, naturally sized incident cards, the repository
table-to-card transition, every shared-storage label/value, and zero horizontal
document or attribution scrolling. The current focused interaction pass has
138 checks and zero failures; formal verification checked all 9 planned cells
with zero critical findings, and all 18 viewport/full-page images passed the
manual review manifest.

The Deployments attribution gate gives different repositories distinct managed
and observed deployments. Switching repositories must replace the collection
without mixing records, while each lifecycle action and domain editor still
addresses its original deployment. The responsive pass checks small phones,
intermediate widths, and desktop layouts with the shared sidebar present.
