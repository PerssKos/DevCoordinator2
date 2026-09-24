## UI design admission gate

This gate applies to every new shipped product UI element or visual asset,
including user-facing, admin, Console, operational, and responsive surfaces:
pages, routes, windows, dialogs, sheets, panels, menus, forms, controls, cards,
navigation, icons, illustrations, and other visible elements. Documentation-only
layouts and developer-only tooling that is not shipped as product UI are outside
this gate.

Before implementation, load and follow these exact skill contracts:

- `$imagegen` (resolve its installed `SKILL.md` through the active skill
  catalog)
- `$product-design:index` (resolve its installed `SKILL.md` through the
  active plugin/skill catalog)
- `$product-design:ideate` (resolve its installed `SKILL.md` through the
  active plugin/skill catalog)
- The Product Design `get-context` skill required by the index and ideate
  contracts.

Resolve the minimum product and journey brief before ideation. Use the
built-in Image Gen workflow and generate exactly three independent visual
options. Options must differ in layout, hierarchy, interaction model, or
product framing; color-only variants do not count. Attach available project
screenshots, tokens, design-system references, mockups, and other visual
inputs according to the loaded skill contracts.

Use the highest model or effort capability the current runtime actually
provides as the accepted equivalent of “Sunburst or higher.” Record the actual
model/effort capability in the design evidence. Never claim that a named
capability was used when the runtime did not provide it.

Present the three generated options to the user in the order the results are
actually displayed. Keep that display order bound to the retained evidence;
submission order, completion order, retries, and array indexes do not define
the user-facing option number.

Pause all implementation while the admission gate is pending. Do not edit product
code, scaffold, start a preview, run implementation work, or publish a build.
Read-only discovery and preparation of the three design artifacts may continue.

Resume implementation only after one of these conditions is recorded:

1. The user selects one displayed option; or
2. The user explicitly authorizes autonomous selection, after which the agent
   selects the strongest option, records the authorization and rationale, and
   continues without another approval round.

Retain all three options, their brief/context, generated asset identities,
actual model/effort capability, displayed order, selection or autonomous-choice
state, actor, timestamp, and rationale in the configured Coordinator's existing
sketch/evidence and decision records. Do not create a Markdown approval ledger,
local selection file, or competing approval service. If the required skill,
Image Gen capability, or Coordinator evidence path is unavailable, leave the
gate pending and report the concrete blocker.

An existing approved visual target may guide faithful implementation, but it
does not waive this gate when the work introduces a new visible element or
materially recomposes an existing one. A repair that changes no visible
element and does not introduce a new visual decision remains ordinary work.

## Post-implementation mockup audit gate

This completion gate applies to every shipped UI implementation backed by a
confirmed mockup or other approved visual target, regardless of who designed or
built it. It covers user-facing, admin, operational, responsive, and native
surfaces. Without a confirmed target, this comparison gate does not apply;
journey and interaction verification still apply. An approved target that
cannot be retrieved is a blocker, not an absent-target exemption. This gate
does not reopen design selection for repairs that restore the approved target.

Before final handoff or a completion claim:

1. Resolve the selected visual target and its journey from the current
   Coordinator sketch/decision and project requirements. Bind the audit to the
   exact source identity, version, and implementation state being reviewed.
2. Load and follow `$product-design:audit` and its index, user-context, and
   critical-overrides contracts, including the user-context preflight when
   local shell access is available. Run its combined UX/design/accessibility
   audit. Also load `$product-design:design-qa` for paired visual comparison
   and the `design-qa.md` report. Resolve both from the active skill/plugin
   catalog, not a version-specific cache path. Design QA alone does not satisfy
   the required audit.
3. Open the exact retained approved source and capture the current rendered
   implementation. Match viewport, route, state, theme, content condition
   (using the same fixture or documenting why live data differs), density, and
   auth conditions. Keep the approved mockup as the reference; do not
   regenerate it or substitute an old implementation screenshot. Put both
   images in the same comparison input, normalize crop and density, and
   inspect each saved capture before accepting it. Cover the agreed screens,
   flow steps, states, supported themes, and relevant wide/narrow layouts. For
   states the mockup does not show, verify requirements and identify the visual
   comparison limit rather than inventing a reference.
4. Tie each step to its screenshots and observations. Review strengths, UX and
   design findings, accessibility risks, evidence limits, and the real rendered
   behavior of navigation, focus, controls, validation, cancellation, errors,
   persistence, and recovery. Screenshots do not replace the rendered
   interaction pass.
5. Explicitly inspect typography and copy, spacing and layout rhythm, colors
   and tokens, image and asset fidelity, hierarchy, responsive reflow, and the
   interaction states represented by the target. Record the concrete difference,
   its user impact, and the fix for every finding.
6. Classify findings as P0 (blocking use or severe accessibility failure), P1
   (major visual or usability mismatch), P2 (moderate visual, responsive, or
   state drift), or P3 (minor polish). P0, P1, and P2 are significant findings
   and block handoff. P3 findings may remain only as an explicit follow-up list.
7. A complete first audit may pass when it finds no actionable P0-P2 issue and
   makes no visual fixes. Otherwise fix in-scope findings, capture the revised
   implementation under the same conditions, and repeat the audit until clear.
   Each later pass links earlier findings, fixes, and post-fix evidence. A fix,
   successful build, or exhausted iteration count is not a passing audit.
8. If a required skill, approved source, implementation, capture tool, or
   required evidence is unavailable, report the exact blocker and keep final
   handoff blocked while independent authorized work continues.
9. Accept an intentional deviation only with a rationale citing an applicable
   confirmed requirement or user-authorized decision. Material changes to the
   selected direction require renewed user selection or approval. Do not
   rewrite the reference or downgrade findings merely to obtain a pass.

Save the report in project-root `design-qa.md`, with source and implementation
identities, step-linked screenshots, comparison conditions, findings, iteration
history, accepted-deviation rationale, evidence limits, and final checklist.
Retain the report and captures through Coordinator evidence and link the source
decision and audit iterations. Reports are evidence, not a separate outcome or
approval ledger; diagnosed gaps follow the existing ledger rules. Render the
accepted screenshots and numbered step verdicts in a concise inline audit
report unless the user has selected another format.

Require `final result: passed` before final handoff, with no actionable P0-P2
finding or required evidence gap and with rendered interaction verification
complete. Otherwise record `final result: blocked` and the reason. Documented
P3 polish is optional follow-up; missing agreed functionality cannot be called
polish. Preliminary previews remain available and clearly incomplete.

Reuse compatible formal-browser and interaction evidence with its actual source
and state bindings, but every new audit run uses fresh implementation captures.
After a passing audit, repeat visual review for affected declared UI code,
styles, tokens, fonts, assets, journey or theme intent, or route/state/viewport
changes. Dynamic pixel drift, unrelated backend changes, and routine invisible
repairs outside those inputs do not reopen it. A changed approved target also
requires comparison. Earlier gaps remain blocking until repaired and reviewed;
an unavailable capture can be retried once access is restored.
