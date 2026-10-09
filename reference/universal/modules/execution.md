## 5. Coordinate tools, delegated work, and asynchronous execution

### UI implementation admission

- Before any implementation batch that introduces or materially recomposes a
  shipped product UI element, verify that the admission portion of
  `ui-design-gate` has completed.
- While admission is pending, discovery and preparation of the three design
  artifacts may continue, but product edits, scaffolding, preview startup,
  implementation tests, and delivery work are paused for that UI scope.
- A displayed user selection or an explicitly recorded autonomous-selection
  authorization is the admission evidence. A proposed option, generated image,
  implementation plan, or agent preference is not selection evidence.
- If the design skill, Image Gen capability, or Coordinator evidence path is
  unavailable, preserve the pending gate and report the blocker; do not bypass
  it with a code-first implementation.

### Mockup history and current-source resolution

- Publish each generated image with an agent-authored manifest for one
  surface/window, state, theme, and viewport. Multiple controls inside that
  window are listed in the manifest; multiple windows or visual alternatives
  require separate files.
- Keep every batch option visible, including rejected options. A continuation
  may select multiple options, and the selected set must be recorded explicitly
  rather than inferred from the newest timestamp or a stale Keep flag.
- Before implementation or visual audit, call the Coordinator's
  `design.sketch.resolve` operation and carry the returned surface, node IDs,
  description revision, and graph revision into the work. After another image
  generation call or user adjustment, resolve again before continuing.
- If resolution is ambiguous, unavailable, or legacy-only, stop dependent UI
  work and preserve the evidence until an explicit current selection exists.

### UI handoff audit

- For UI backed by a confirmed mockup or approved visual target, verify that
  the post-implementation mockup audit in `ui-design-gate` has passed before
  reporting completion or handing the surface off.
- A missing or blocked `$product-design:audit`, unavailable comparison
  evidence, or any unresolved P0-P2 finding keeps the UI incomplete. Fix the
  affected surface and repeat the audit under the same comparison conditions.
- A preliminary preview may remain available for inspection while this gate is
  open, but it must be described as preliminary and must not be presented as
  final visual approval.
- This gate supplements the rendered interaction and end-to-end evidence; a
  screenshot comparison cannot prove that enabled controls, persistence,
  recovery, or downstream integrations work.

### Tools and asynchronous work

- Partition tool calls into dependency layers. Execute safe independent
  calls in the same layer concurrently; serialize only real dependencies,
  semantic decisions, approvals, or conflicting mutations.
- Prefer asynchronous or nonblocking execution when supported and useful,
  especially for builds, tests, packaging, publication, and independent
  discovery. Start authorized dependency-ready work, then continue other
  useful work instead of waiting unnecessarily.
- Await a result when it is needed for the next dependent action, a
  consequential decision, or final verification—not merely because a
  background operation is running.
- Use bounded programmatic orchestration for mechanical pagination,
  filtering, joins, deduplication, and aggregation. Return compact
  structured conclusions, evidence, and errors.
- Delegate implementation only after shared schemas, directory layouts,
  ownership boundaries, and one cross-component acceptance fixture are
  fixed. Independently ready work has no unresolved shared-interface
  decision or overlapping mutable-file ownership.
- Choose delegation according to genuinely independent work, clear
  ownership, and integration needs rather than a fixed implementation-
  agent count. The parent remains the sole integration owner. Nested
  implementation delegation requires explicit parent authorization for
  that independent branch.
- Submit governed dependency-ready work to the configured host-wide
  scheduler. Do not invent local worker limits, fake dependencies, or
  a second capacity controller.
- Make cheap checks that can invalidate expensive downstream evidence real
  success dependencies. An ordinary failure does not cancel safe siblings.
  If the harness cannot express required concurrency, record the missing
  capability and use the best supported execution without false claims.
- Asynchronous execution is not fire-and-forget. Retain operation
  identities, observe completion and failures, preserve evidence, and
  perform required cleanup. Submission alone is never proof of success.
- Before claiming completion, account for every required background
  operation and verify its result. Do not leave necessary work running
  unobserved or imply ongoing execution that has not been established.

### Await required asynchronous work

Required running or queued work remains part of the active task. A tool yield
continues the wait; a final response ends the active work cycle and is not a
substitute for waiting.

1. When launching required asynchronous work, retain its operation/run ID,
   source identity, expected terminal outcomes, and next dependent action.
   Continue independent authorized work while it runs.
2. Before waiting, read the operation's current status. If already terminal,
   inspect its result and continue immediately. Preserve the last observed
   sequence or cursor so completion between the status read and subscription
   is not lost.
3. When its result is needed, use the runtime's event-wait tool, such as
   `await_work` when exposed, only if the provider is confirmed connected to
   that runtime's event ingress. Use the provider's actual event types, exact
   operation labels, last observed sequence, and a meaningful deadline.
   Naming a source does not establish an event integration. Multiplex pending
   subscriptions and due heartbeats through the existing shared scheduler.
4. Verify that registration succeeded and retain the returned subscription
   identity or supported wait handle. After a tool yields, remain in that
   wait through its supported continuation. Do not substitute repeated
   sleep/status-query loops or a final response.
5. If event delivery is unavailable, use the provider's supported blocking
   wait, such as Coordinator `test wait`. If that is unavailable, use one
   existing service-owned watcher with bounded backoff, a deadline, and
   cancellation. Deduplicate watchers for the same obligation, choose backoff
   for the source and responsiveness need, and reset it on meaningful state
   change. Do not add a scheduler, poll through model turns, or repeatedly
   resubscribe to a known unavailable source.
6. On wake, read bounded authoritative status once and advance the cursor.
   Inspect terminal results, preserve failures, perform required cleanup,
   and take the next authorized action. A deadline, disconnection, or
   source-unavailable event does not mean the operation completed. Diagnose
   the current state and supported recovery before renewing a wait; do not
   cancel or relaunch accepted work merely because observation ended.
7. Before sending a final response, account for every required outstanding
   operation. Continue waiting unless the user stopped the task, a genuine
   external blocker prevents both progress and supported waiting, or an
   explicitly authorized background handoff has a verified wake route.
   Confirm any required background-wake permission; an alarm or subscription
   registration alone does not grant it. Without a working continuation
   route, state the missing capability and required unblock action instead
   of promising automatic resumption.

Use the runtime's durable alarm tool, such as `alarm_set` when exposed, for
deadline reminders or supported exact-operation terminal triggers. Do not
attach a completion alarm to a launch command and assume it observes the
external job's eventual completion. A reminder does not replace the wait or
the terminal-result check.

### Continue development while testing is unavailable

1. Inspect the refusal and its typed recovery guidance, including whether
   independent work is safe, whether the condition is temporary, and which
   wait operation is supported. A refused launch is not queued work and has
   no accepted run to await. For example, a Coordinator upgrade or Rust
   cutover can close test admission while existing accepted runs drain.
2. Continue independent authorized development, test authoring, source
   review, and permitted static checks while runtime testing is unavailable.
   Keep the affected acceptance checks and source identities explicit in
   existing task/evidence records; defer execution, not the requirement to
   verify. Preserve frozen candidates, ownership, UI admission, security,
   delivery hard stops, and the prohibition on self-hosted Coordinator
   validation. Do not bypass the refusal with an unmanaged test runner or
   assume an untested prerequisite succeeded.
3. When no independent work remains, keep the task active and apply the
   completion-wait procedure above to the required availability transition.
   Use a confirmed availability event source or the provider's named blocking
   wait. For Coordinator `tests_draining`, inspect `test.admission.status`,
   then use `test.admission.wait` (CLI `test admission-wait --deadline-at ...`)
   with a real deadline if admission is still closed. A provider wait does
   not require a separate runtime event integration. If the service cannot
   accept the wait, use its existing external availability watcher when
   supported; do not invent a source or repeatedly submit refused tests.
4. After availability returns, read the authoritative state once, reconcile
   any accepted runs, and submit the deferred checks for the intended source
   candidate. A refused request must be submitted again after recovery; it
   will not start by itself. Observe each accepted run through completion,
   inspect its results, and repair failures before claiming verification.
5. A wait deadline or lost connection requires diagnosis, not a completion
   claim or automatic abandonment. Resume a supported wait when recovery is
   still expected and the deadline is justified by current evidence. If
   recovery needs an unavailable capability or external action and no
   supported wait remains, preserve the unfinished outcome and explain the
   exact blocker, missing verification, and smallest unblock action. Never
   claim automatic continuation without the confirmed route and permission.
