## 6. Deliver preliminary results on a regular cadence

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

- Prepare meaningful, runnable increments for an authorized non-production
  surface: a test server, application build, executable, or other appropriate
  inspectable result. Publish them on the cadence below without waiting for
  the whole feature or broad release validation. Batch small changes instead
  of requiring an expensive deployment after every implementation batch.
- A deployment or release row is not, by itself, qualified delivery evidence.
  For a delivery-clock target, use the Coordinator's retained-evidence path:
  complete a governed run, retain the artifact, create the bounded
  `release.deliver_evidence` request with its compact verification document,
  read the returned receipt, require `qualified: true`, and pass that receipt
  ID to `project_automation.record_delivery`. `release deliver` records
  deployment metadata only and must not be used as the delivery evidence
  reference. Do not pass a preview directory or a full screenshot/journey
  bundle as the bounded request; the request is separate from the retained
  artifact and the verification file inside that artifact.
  The request names the exact release, repository/worktree path, governed run,
  check, retained artifact, manifest digest, source digest, target, delivery
  kind, and verification-file name. The verification file is a separate
  bounded `Verification` document inside the retained artifact; it carries
  the observed file digest, access URL, checked timestamp, and exact web
  deployment generation when the kind is `web-deployment`.
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
- At the effective delivery interval, the runtime alarm requests one delivery
  concurrently. Plan the first qualifying result at the next convenient,
  meaningful point; the alarm is not a per-batch deployment command. Measure
  the first alarm from the delivery-eligible implementation start and later
  alarms from the last qualified delivery receipt. Use elapsed UTC time, not
  accumulated agent working hours. The qualifying result must be published
  before the hard-stop threshold unless the bounded-operation allowance below
  applies. Short tasks still finish under their normal acceptance criteria;
  do not prolong them to reach an alarm.
- Register the alarm and later hard-stop threshold once per target and baseline
  through the runtime's existing scheduler. At the alarm, identify the coherent
  result to deliver, its remaining work, and a convenient delivery point before
  the later deadline. The first alarm never blocks ordinary implementation or
  demands immediate deployment. Start or attach to one delivery request for
  that target and deadline revision; unchanged wakes do not create duplicates.
  After qualified delivery, schedule the next alarm from its actual delivery
  time. Do not create a separate scheduler or grant background wake permission
  merely by registering a clock.
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
  threshold resumes with delivery recovery, including only the bounded
  unfinished work allowed below. Resuming does not authorize a fresh batch or
  reset the deadline.
- Use available scheduling and event mechanisms to observe deadlines during
  work, without adding a competing scheduler or agent status-polling loop.
  Record actual operation identities; an instruction or promised schedule is
  not proof of a running watchdog. Report missing required capabilities
  explicitly instead of claiming unattended enforcement exists.

### Keep web applications available

- After the delivery alarm, deploy one coherent web-application increment
  through the configured DevCoordinator service at the next convenient point
  and before the hard-stop threshold, subject to the finishing allowance below.
  Publish a stable URL that the user can actually access, and verify the
  advertised behavior there.
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

- After the delivery alarm, build one coherent set of usable packages for every
  agreed platform and architecture at the next convenient point and before the
  hard-stop threshold, subject to the finishing allowance below. Publish
  verified downloads through a DevCoordinator-hosted web server, under the
  agreed public domain when available. Identify the source snapshot and version
  for each package.
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

- Delivery timing follows elapsed time, not batch size. Keep the current
  preview available while preparing the next coherent result. An earlier
  delivery is appropriate for an explicit user request, a useful milestone,
  completion, or restoration of a broken preview; routine small edits do not
  require repeated expensive delivery. Do not delay a requested or completed
  result just to wait for an alarm.
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
- A successful `release deliver` response, a healthy deployment, or a passing
  journey run without a qualified `release.deliver_evidence` receipt does not
  reset a delivery deadline. If the receipt cannot be produced, preserve the
  preview and evidence, diagnose the exact contract failure, and keep the
  delivery obligation open.
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

### Finish current bounded work and deliver when overdue

- A queued or failed build, compilation without accessible downloads, an
  inaccessible deployment, a status report, or reposted stale artifacts is not
  a qualifying delivery. Do not reset a deadline until the result is available
  to the user and its advertised behavior has been verified.
- At or beyond the effective hard-stop threshold without a qualifying delivery,
  block the affected scope from starting a new implementation batch, including
  delegated work; independent scopes continue. Both thresholds use the same
  baseline: 36 hours is not 36 additional hours after the 24-hour alarm. Keep
  the overdue condition visible; one target's delivery cannot clear another's.
- The agent may finish the bounded work item already in progress at that
  threshold when finalizing it directly makes the pending delivery usable.
  Identify its remaining work and completion point in the existing delivery
  request, finish it, and deliver immediately afterward. This includes edits,
  necessary integration, checks, packaging, publication, and cleanup for that
  same result, even when those commands were not already running. No separate
  permission is needed within the authorized scope; mandatory host/tool
  controls still apply.
- This allowance cannot expand the current item, relabel the whole unfinished
  project as current work, begin another feature or batch, or defer delivery
  for optional refinement. It adds no third deadline, does not reset the clock,
  and does not clear the overdue state. If the item cannot be finished within
  its stated boundary, preserve it and continue delivery recovery instead of
  chaining further implementation batches.
- Apart from that bounded finishing allowance, allow only delivery recovery,
  necessary diagnosis and repairs, supporting checks, reporting, and
  preservation of existing results. Resume ordinary implementation only after
  a qualified delivery is verified or the user explicitly changes the
  obligation.
- Record the exact recovery condition through the existing Coordinator
  records and communicate it to all affected agents. Resume implementation
  only when the missing delivery obligations are verified as restored or the
  user explicitly changes the applicable obligation. Merely queuing a retry,
  acknowledging the delay, or posting a report does not clear the stop.
