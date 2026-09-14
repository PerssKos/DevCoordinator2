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
- On the first ordinary failure, preserve the failing input, observed state,
  and evidence. Begin investigating both the originating defect and how its
  effects propagate while the original run continues.
- Continue the failing test or scenario through every remaining safe,
  meaningful observation, including error handling, cleanup, rollback, and
  recovery. Keep independent cases running. Design tests to collect multiple
  assertion failures when execution can continue meaningfully; never swallow
  failures or convert them into success.
- Trace the actual propagation of invalid data, partial state, or incorrect
  behavior through relevant downstream consumers. At each boundary, establish
  what was received, what the contract required, what actually happened, and
  whether the failure was detected, contained, transformed, silently accepted,
  or amplified.
- Verify the consequences: incorrect persistence or cached state, misleading
  API/UI results, duplicate actions, leaked resources, failed cleanup, and
  recovery that leaves inconsistent state. Investigate secondary failures in
  handling and recovery as distinct defects linked to the initiating failure.
- Preserve truthful results. A failed prerequisite can invalidate later
  success assertions while still allowing meaningful checks of failure
  handling. Mark unsupported conclusions as blocked or inconclusive. Never
  treat corrupted-input behavior as evidence that the normal success path
  passed.
- If a process crashes or the harness cannot continue, preserve that result
  and examine downstream handling using separate isolated reproductions or
  controlled fault injection. Do not modify the sealed run, fabricate
  successful prerequisites, or bypass production validation controls to force
  progress.
- Stop or contain only the affected branch when continuation threatens
  safety, valuable data, shared state, or evidence. Deliberately invalid state
  inside disposable fixtures is not itself a reason to abandon diagnostic
  exploration. Continue safe observations and unaffected branches.
- Follow each relevant propagation path until containment, recovery, or a
  user-visible consequence is demonstrated, or identify the exact evidence
  gap. Prefer observations that distinguish competing explanations; avoid
  repetitive cascaded errors that add no information.
- After the pass, distinguish the initiating defect, expected downstream
  rejection, broken downstream handling, independent failures, and unverified
  paths. Retain concise causal findings with evidence references. Batch
  related repairs and add regressions for both the original defect and the
  downstream handling failures before final validation. Use focused checks
  during repair, then run one final complete pass over the final frozen
  candidate.
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
