## 9. Verify UI journeys and put requested content first

### Build around user journeys

- Organize the interface around what users need to accomplish, not around
  database entities, API endpoints, services, implementation modules, or
  the order in which features were developed.
- For the affected work, identify the user's starting situation, intended
  outcome, necessary steps, meaningful decisions, and recovery or
  cancellation paths. Reuse established journeys rather than inventing a
  separate discovery exercise for routine changes.
- Derive navigation, destinations, content groups, controls, and feedback
  from those journeys. A technical component or data structure does not,
  by itself, justify a page, tab, section, form, or visible concept.
- Each destination must have a clear user purpose. Users should understand
  what they can accomplish there, what requires their attention, and what
  to do next without knowing how the application is implemented.
- Present technical concepts only when they are genuinely part of the
  user's task. Distinguish domain information users need from internal
  machinery they should not have to understand.

### Design alternatives and contextual interfaces

- Generate exactly three materially different visual options before
  implementing a new interface or substantial redesign. Change layout,
  information hierarchy, or interaction, not merely colors. Ground all three
  in the user's journey and the existing design system.
- Normally, present all three and recommend one, then obtain the user's
  selection before implementing. If the user explicitly asks the agent to
  choose the best option or proceed autonomously, select the strongest of the
  three, briefly explain the choice, and implement without another approval
  round. Preserve previously approved designs; routine fixes do not require
  three new proposals.
- Persist the options, selection or approval state, and exact outstanding
  response request, if any. Include that state with the visual artifacts when
  no follow-up can appear. Do not invent a pending approval when the user has
  authorized autonomous selection or reopen an already approved design.
- Minimize effort and preserve context. Inherit known project, parent, and
  other values. Show infrequently changed context as clickable text rather
  than permanent full-size selectors. Keep actions beside the object they
  affect, including useful actions in empty states.
- Prefer direct, compact controls. Use one-click choices with recognizable
  icons and labels for small option sets; visible labels may collapse under
  the responsive rules below. Reveal optional fields on demand. Dropdowns
  must overlay content rather than stretch forms.
- Show meaningful results, not explanatory clutter. Provide live previews
  when choices generate a part number or other output. Put detailed
  explanations and examples behind small contextual help buttons.
- Verify the chosen design through actual use. Check creation, cancellation,
  errors, persistence, keyboard/touch interaction, and responsive layouts in
  every supported theme. Screenshots alone do not establish that the interface
  works.

### Preserve rows and use vertical space efficiently

- Collapse action labels before wrapping, clipping, or overlap; restore them
  when space returns. Preserve accessible names and usable touch targets.
- Keep static page elements, including headers, toolbars, navigation, and
  action groups, in their existing rows as the page or container narrows.
  Reduce unnecessary horizontal gaps and collapse secondary visible labels
  rather than stack these elements merely to retain their text.
- Keep recognizable icons, programmatic action names, keyboard access, and
  focus when visible labels collapse. Retain essential wording when an icon
  alone would be ambiguous; hover-only descriptions are not a substitute for
  understandable touch interaction. Do not shrink usable targets or make
  text unreadable to force a fit.
- Always look for ways to save vertical space. Combine related controls into
  compact rows, remove redundant headings and status strips, reduce excessive
  gaps and padding, and reveal optional details on demand. Keep the user's
  primary content prominent without sacrificing readable grouping, essential
  guidance, or comfortable interaction.
- Apply row preservation to static page structure, not as a blanket no-wrap
  rule for body text, user data, or forms. Reflow a static row only when it
  still cannot fit after label collapse and spacing reduction while keeping
  essential meaning and usable controls. Never substitute clipping, overlap,
  lost actions, or page-wide horizontal scrolling for a usable layout.
- Verify both shrinking and expanding through the actual label-collapse
  transitions, including text zoom, long translated labels, and the smallest
  supported layouts in every supported theme. Labels must return when space
  permits without losing state or focus; inspect intermediate widths, not
  only preset desktop and mobile sizes.

### Keep development commentary out of the product

- Do not turn implementation notes, agent instructions, design guidelines,
  QA observations, development progress, or explanations of engineering
  choices into ordinary product UI.
- Keep those materials in their appropriate documentation, development
  tools, or progress reports. They belong in a product screen only when
  reviewing that material is itself an explicitly intended user task.
- Do not describe how a feature was built when the user needs to use it.
  Communicate the available action, relevant result, or necessary next
  step instead.
- Labels, grouping, sensible defaults, direct controls, and observable
  behavior should carry the experience. Do not compensate for confusing
  design with explanatory paragraphs.

### Require a concrete purpose for UI text

Before adding status text, helper text, descriptions, explanations, banners,
or instructional copy, apply this internal design check:

1. Who needs this information at this point in the journey?
2. What action, decision, result, or error does it help them understand?
3. Can someone understand it without knowing the application's internals
   or development history?
4. Is the information already clear from the label, layout, current state,
   or nearby content?
5. Would a clearer label, better default, simpler interaction, or improved
   placement remove the need for the explanation?

- If the text has no concrete user-facing purpose, omit it. Do not merely
  rewrite unnecessary technical commentary in simpler language.
- Prefer one concise heading or label; add supporting copy only when
  requested or necessary to prevent misunderstanding or error.
- Do not add copy to fill space, restate headings, narrate obvious controls,
  advertise implementation completeness, or explain internal architecture.
- Show status when it affects the user's understanding or next action:
  meaningful progress, a blocking condition, a relevant result, or a
  failure with a useful recovery step. Avoid redundant persistent status
  messages when the interface already makes the state clear.
- Keep necessary guidance concise, specific, and beside the action or
  object it supports. Reveal advanced explanations when needed rather
  than making every user read them.
- This is an agent-owned design check, not a requirement to ask the user
  to approve each piece of copy.

### Minimize surfaces and handoffs

- Use the fewest coherent destinations, modes, dialogs, tabs, and steps
  needed to complete the agreed journeys comfortably.
- Prefer actions and details in the user's existing context. Do not create
  another page merely because another entity, endpoint, or implementation
  component exists.
- Every additional surface must serve a distinct user purpose and provide
  a clear advantage over extending an existing journey in place.
- Avoid duplicate dashboards, overview pages, detail pages, settings panels,
  and status sections that make users visit several places for one task.
- Minimize user effort, not URL count alone. Do not collapse distinct tasks
  into an overloaded screen or hide essential actions merely to reduce
  the number of pages.
- Review the completed journey for unnecessary navigation, repeated entry,
  context loss, competing actions, duplicated information, and copy that
  exists only to explain the design.

### Interaction completion

Before reporting UI complete, finish one evidence pass over only the agreed
screens, journeys, states, and responsive variants. This does not authorize a
broader exhaustive audit.

1. Inventory every visible interactive element, including conditional ones.
2. Map each to its journey, action, and expected observable result.
3. Invoke it through the rendered interface and verify the downstream result.
4. Exercise success, cancellation, validation failure, permission failure,
   and recovery where applicable; reload when persistence is promised.
5. Record gaps and finish the safe diagnostic pass. Isolated repair may
   proceed under the sealed-run rules; reconcile, batch-fix, and rerun.
6. Require zero enabled controls without real behavior, zero requested
   journeys without rendered end-to-end evidence, and zero request-related
   unfinished outcomes.

Code inspection, routes, rendering, screenshots, visual comparison, and
geometry checks support evidence but do not replace interaction verification.

### Content and interaction design

- Before changing UI wording, read the effective project glossary and
  applicable shared terminology from their established sources. Resolve
  vocabulary by concept and language, respect required inherited rules, and
  explain project specializations rather than inventing competing names.
  Projects retain their localization architecture and exact messages;
  glossaries guide meaning and vocabulary, not storage or sentence assembly.
  Resolve missing concepts and language-review gaps explicitly. Reading the
  glossary is not proof of UI compliance: verify terminology in the affected
  user-facing result, preserving legitimate grammar, names and quoted content.
- A destination's name is a content promise. Its named object, collection,
  task, or honest loading/error/empty state must be the first substantial,
  recognizable content in the initial viewport, including narrow screens.
- A compact title, breadcrumb, count, search, filter, sort, or critical
  blocking alert may precede it only when supporting rather than
  displacing the requested content.
- Collection destinations do not lead with add or edit forms. Put creation
  actions beside the collection heading or toolbar. Forms may lead on
  destinations explicitly dedicated to creating or editing one item.
- Creation immediately reveals a focused dialog, narrow-screen sheet,
  dedicated page, or deliberately placed inline editor in the current
  viewport—not below a long list. Success reveals the new item in its
  collection; cancellation restores context and focus.
- Rank other content by current-goal relevance, frequency, expected
  location, and justified space. Keep controls beside the affected object
  and activation, preview, editing, selection, and destruction distinct.
  Destructive actions name an explicit target and state.
- Show a simple normal first input before inferred or advanced fields.
- Do not expose private values, internal identifiers, serialized payloads,
  or implementation invariants as normal UI content. Use validated,
  purpose-built controls for editable concepts.
- Verify representative wide and narrow layouts across loading, empty,
  error, populated, and long-content states. Test creation after a long
  list, immediate visibility and focus, saving, and the new item in context.
  Hidden, clipped, overlapping, inaccessible, misleading, or displaced
  primary content is a functional defect.
