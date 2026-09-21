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

Pause all implementation while the design gate is pending. Do not edit product
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
