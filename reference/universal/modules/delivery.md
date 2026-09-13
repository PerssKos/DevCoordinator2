## 6. Deliver preliminary results continuously

### Resolve project delivery deadlines

- Classify each actual project/workstream before starting clocks. Specifications,
  research, and work without a meaningful deployable result are performance-only:
  they have performance reviews, no delivery targets, obligations, or alarms.
  Project age alone never creates a delivery obligation. Do not implement a
  product to satisfy a deadline during a specification-only request. An explicit
  authorized implementation transition with a real target starts delivery timing.
- The agent runtime owns durable clocks, deadline revisions, deduplicated wakeups,
  and work admission. The Coordinator owns authoritative outcomes, decisions,
  evidence, and capacity. Do not add scheduling or agent execution to its API.

- Throughout implementation, expose the earliest meaningful, runnable
  increment on an authorized non-production surface: a test server,
  application build, executable, or other appropriate inspectable result.
  Do not wait for feature completion or broad validation.
- For each delivery-eligible project/workstream target, resolve the delivery
  interval and overdue hard-stop threshold from applicable project
  instructions and user-confirmed Coordinator decisions. Project-specific
  values take precedence. Resolve each setting independently: when absent,
  the delivery interval defaults to 24 hours and the hard-stop threshold
  defaults to 36 hours. These are fallback defaults, not fixed mandates.
- Both intervals must be positive, and the hard-stop threshold must not be
  shorter than the delivery interval. Resolve conflicting or incompatible
  declarations with the user rather than silently choosing a more permissive
  value. Never extend an interval for agent convenience. Explicit user
  postponement, pause, or threshold changes revise the applicable obligation,
  retaining the prior revision, reason, confirmation, and actual timestamps.
  Invalidate obsolete timer wakes and immediately reevaluate existing blocks;
  a superseded deadline cannot keep work blocked. Silence or a failed build is
  not a postponement. Review timing is unchanged unless revised separately.
- Deliver the first qualifying result within the effective delivery interval
  measured from the delivery-eligible implementation start, including its
  discovery and setup but excluding earlier performance-only work. Thereafter,
  measure each deadline from the last qualifying delivery. Use elapsed UTC
  time, not accumulated agent working hours. Short tasks still finish under
  their normal acceptance criteria; do not prolong them to reach a checkpoint.
- Preserve actual work-start and implementation-transition times, effective
  intervals, deadline revisions, and wake identities in the runtime clock state;
  reference authoritative Coordinator delivery evidence, decisions, and reports.
  Qualified deliveries and completed reviews are separate receipts and cannot
  reset one another. Read delivery times from actual delivery evidence, not the
  time a status message was posted. Do not invent tool fields or ledger copies.
- Starting a new session, changing agents, creating subtasks, or handing off
  work does not reset a project deadline. Check the shared deadlines before
  starting or resuming implementation and before each new work batch. When
  the user changes an interval, retain actual timestamps and apply the newly
  agreed interval; do not fabricate a new start or delivery event.
- An explicit user pause does not require continued execution. Keep the
  existing preview available unless the user asks otherwise, and check its
  actual age on resumption. Work already beyond its effective hard-stop
  threshold resumes with delivery recovery, not further implementation.
- Use available scheduling and event mechanisms to observe deadlines during
  work, without adding a competing scheduler or agent status-polling loop.
  Record actual operation identities; an instruction or promised schedule is
  not proof of a running watchdog. Report missing required capabilities
  explicitly instead of claiming unattended enforcement exists.

### Keep web applications available

- Deploy web-application increments through the configured DevCoordinator
  service at least once per effective delivery interval. Publish a stable URL
  that the user can actually access, and verify the advertised behavior there.
- Keep the intermediate server continuously available for inspection while
  development continues. Preserve the last working version while preparing
  its replacement; restore a failed preview promptly. Do not tear it down
  merely because an agent or task ends. Retire or replace it only through an
  agreed change that preserves the user's intended access.
- When public-domain assignment is available, use a domain agreed with the
  user. Reuse an existing agreement instead of requesting it for every update.
  While a new domain decision is pending, provide an already authorized,
  accessible URL rather than withholding the preliminary result. A public
  hostname does not by itself authorize anonymous access or removal of controls.

### Deliver usable desktop builds and updates

- Build usable packages for every agreed platform and architecture at least
  once per effective delivery interval. Publish verified downloads through a
  DevCoordinator-hosted web server, under the agreed public domain when
  available. Identify the source snapshot and version for each package.
- Track qualifying delivery separately for every required target. A successful
  build for one platform does not reset another platform's deadline. Surface
  missing build or distribution prerequisites early; never silently drop a
  platform, substitute an unsupported package, or claim an unavailable build.
- Automatic updating must work in the first qualifying desktop delivery.
  Check for updates on startup and periodically while running: every hour by
  default unless another interval is agreed for the project. Download an
  available update in the background without interrupting ordinary use.
- Once the update is downloaded and passes the project's established update
  verification, show a small `Update` button in the window caption. Activating
  it installs the update and restarts the application. Do not restart without
  that action or discard unsaved work. A failed check, download, or installation
  must leave the current version usable or recover it through the reviewed
  update mechanism.
- Verify the actual download and update path, including compatibility with
  the agreed access controls. An updater stub, fake ready state, inaccessible
  feed, or unexercised update button does not satisfy the delivery requirement.
  Keep existing credential, artifact-verification, and trust controls intact.

### Publish without pausing independent work

- Refresh the available result promptly as coherent, runnable increments
  become available. Preliminary delivery is a continuing development
  activity, not a one-time preview or final handoff. The delivery interval is
  a maximum gap, not a reason to delay an earlier useful result.
- Keep implementation, focused testing, packaging, publication, and broader
  validation moving concurrently wherever independent. Publication and
  user inspection must not gate unrelated work unless the project's overdue
  stop applies. Build and publish an identified, stable source snapshot while
  the development checkout continues evolving. Serialize only genuine
  dependencies, conflicting mutations, or safety constraints.
- Before exposing an update, run the narrowest relevant checks needed to
  establish that the increment is safely runnable and its advertised
  behavior works. UI increments include the affected rendered interactions.
  Do not make complete release validation a prerequisite for preliminary
  delivery; frozen validation continues to prove only its own candidate.
- Keep the user able to inspect and guide the work throughout development.
  Incorporate feedback promptly within the agreed scope. Do not wait for
  acknowledgement unless a material decision genuinely requires it.
- Report every qualifying delivery with the actual URL or download links,
  version, what the user can try against the agreed requirements, and important
  limitations. Maintain exact access or launch instructions. Label results
  preliminary; they are not final readiness or final visual approval.
- Reuse established surfaces and delivery mechanisms. Respect declared
  shared environments and coordinate actual source, resource, or server
  conflicts rather than creating unnecessary per-agent environments.
- For work that is neither a web nor desktop application, provide an
  appropriate concrete, inspectable intermediate deliverable on the same
  project cadence rather than imposing irrelevant application packaging.
- Honor the coordinator's documented non-self-hosting restriction when
  working on the coordinator itself. Use that repository's reviewed workflow
  without relaxing the delivery deadlines or evidence requirements.
- Standing permission covers in-scope local browser automation and the
  configured development coordinator's local runtime work without separate
  chat authorization. Preserve the tools' documented controls.
- This workflow does not expand scope or authorize production changes,
  destructive data actions, credential or trust changes, new infrastructure
  outside the agreed work, or bypassing host/tool approval controls.
- Preliminary delivery does not reduce the final agreed result. Incomplete
  scope remains explicit and tracked; exposed behavior must remain truthful.

### Stop overdue implementation until delivery is restored

- A queued or failed build, compilation without accessible downloads, an
  inaccessible deployment, a status report, or reposted stale artifacts is not
  a qualifying delivery. Do not reset a deadline until the result is available
  to the user and its advertised behavior has been verified.
- On missing the effective delivery interval, report the overdue result,
  cause, and recovery action promptly. Start or attach to exactly one
  delivery request for that scope and deadline revision while independent
  coding continues. Repeated wakes do not duplicate the request. Being overdue
  before the hard-stop threshold never blocks ordinary implementation; the
  later threshold is not permission to ignore delivery. Both thresholds use
  the same baseline: 36 hours is not 36 additional hours after the 24-hour trigger.
- At or beyond the effective hard-stop threshold without a qualifying
  delivery, block ordinary implementation throughout the affected delivery
  scope, including every delegated agent working in it; independent scopes
  continue. For the first delivery, measure from the eligible implementation
  start; afterward, measure from the last qualifying delivery for each required
  surface or target. Do not conceal an overdue target behind another's success.
- Allow only delivery recovery, necessary diagnosis and repairs, supporting
  builds and checks, reporting, and preservation of existing results. Do not
  start unrelated implementation or optimization under the label of recovery.
  Safe finite runs already in progress may finish preserving their evidence;
  independent projects may continue.
- Record the exact recovery condition through the existing Coordinator
  records and communicate it to all affected agents. Resume implementation
  only when the missing delivery obligations are verified as restored or the
  user explicitly changes the applicable obligation. Merely queuing a retry,
  acknowledging the delay, or posting a report does not clear the stop.
