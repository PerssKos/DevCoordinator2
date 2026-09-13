## 7. Validate at stable checkpoints without disrupting progress

- Do not run tests or automated policy-validation suites for AGENTS.md
  instruction changes. Review the wording, scope, consistency, and diff
  directly. Changes to executable behavior are a separate validation
  decision; an instruction edit alone is not a test trigger.
- During implementation, run cheap checks and focused tests that can
  invalidate the current design or changed behavior. Complete coherent
  implementation batches before broader validation.
- Do not run the complete suite after each plan item, edit, commit, or
  delegated result. Run pre-merge validation once shared interfaces and
  integrations are stable.
- Run complete release validation against a frozen candidate. Keep its
  source, configuration, running surface, and evidence unchanged while the
  mutable development surface continues evolving.
- A run proves only its exact snapshot. New preliminary increments do not
  justify stopping or restarting it, and its evidence does not establish
  readiness for a later candidate.
- On the first ordinary failure in a finite sealed test, audit, rehearsal,
  or deployment run, begin diagnosis and repair immediately in separate
  isolated state, subject to the applicable scope, approval, and
  mistake-prevention gates.
- Let the original run finish collecting every safe finding. Do not inject
  fixes into its running surface, restart it, or destroy its evidence.
  Keep findings in run evidence rather than creating tasks as they appear.
- Stop or mitigate immediately only when continuation risks security or
  safety harm, data loss, shared-state corruption, destruction of useful
  evidence, or results invalid enough to make the remainder misleading.
- After the pass, reconcile all findings, promote only diagnosed durable
  gaps, and batch related fixes. Use focused checks during repair, then run
  one final complete pass over the final frozen candidate.
- One agent owns complete-suite execution. Delegated agents run focused
  checks unless assigned the sealed integration pass. Test-plumbing-only
  changes do not trigger another complete release pass until both the
  implementation and test infrastructure are frozen.
- Derive tests from acceptance criteria and realistic success, edge,
  failure, integration, and recovery paths. Reproduce and retest the same
  visible or operational surface when feasible; do not substitute an
  internal unit for promised end-to-end behavior.
- Every detector, verifier, audit, monitor, or alert must demonstrate both
  recall and precision: realistic must-catch failures for each advertised
  class and false-positive guards for intentional patterns.
