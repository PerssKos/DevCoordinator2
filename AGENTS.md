# DevCoordinator2 Repository Instructions

These instructions apply to every coding agent working in this repository.
The universal cross-repository policy is `reference/universal/AGENTS.md`.

## Canonical checkout and worktrees

- `/home/DevCoordinator2` is the one live source for the daemon, edge, command
  line, six agent skills, and universal policy.
- Keep that checkout on a clean `main` exactly fast-forwarded to
  `origin/main`. Fetch before consequential repository-wide work.
- Develop in a separate linked worktree and merge through Git. Never stash,
  reset, clean, rebase, or develop directly in the live checkout.
- A dirty, stale, non-main, or non-fast-forward live checkout blocks service
  restart and readiness.

## Shared DevCoordinator2 test surface

- DevCoordinator2 is always a non-production test environment for its own
  development. Its existing Console server and port are the default shared UI
  preview surface; agents do not need to ask whether they may publish an
  in-scope preliminary UI correction there.
- Keep one shared server on the same port. Do not create per-agent preview
  servers or ports.
- Assume concurrent agents are working on different pages or URLs unless the
  assigned work or discovered edits show otherwise. They may update the shared
  test surface concurrently and must preserve one another's changes.
- Different pages or routes remain non-conflicting when their implementations
  occupy distinct symbols or source regions inside one file. Coordinate only
  an actual overlapping symbol, hunk, shared component, or incompatible server
  mutation.
- Keep the server running and expose asset-only changes through reload or
  revalidation. When a restart is genuinely required, coalesce the current
  shared changes into one restart so agents do not restart over one another.
- Preserve the canonical clean-main requirement. When serving unmerged UI
  changes, direct the shared server at a designated shared development
  worktree rather than editing the canonical checkout.
- The test-server designation authorizes rapid preview and verification. It
  does not make persistent data, credentials, authorization controls, or
  root-service state disposable.

## Mockup-backed UI handoff

- Use the shared Console preview as the rendered target for every
  mockup-backed UI change. After implementation, run the active
  `$product-design:audit` contract against the confirmed source mockup with
  matching states, viewports, themes, density, and content.
- Keep the paired source/rendered captures, findings, comparison history, and
  final result in project `design-qa.md` and Coordinator evidence, linked to
  the same audit run. Fix and re-audit every P0-P2 finding before final
  handoff; documented P3 polish may remain as follow-up work.
- An unavailable approved mockup, capture, audit, or unresolved P0-P2 finding
  blocks final handoff. A change without a confirmed target follows the
  universal gate's applicability rules. Keep the shared server available and
  describe previews with outstanding audit work as preliminary and incomplete.

## Authoritative project context

- Read `security-assumptions.md`, relevant requirements, and applicable
  recorded decisions, user feedback, and durable corrections before
  consequential work. Use `devcoordinator2 plan overview`, `task history`,
  and `decision tail`/`search`; include relevant resolved feedback and older
  standing corrections rather than relying only on active tasks.
- DevCoordinator2's database is the only completion ledger and decision
  history. Keep unfinished feedback in its task ledger and confirmed
  repeatable corrections in aspect-tagged decision records, linked to the
  relevant feedback and preserved after fixes. Use `devcoordinator2 plan`,
  `task`, and `decision`; never create or update Markdown ledger mirrors.
  Historical files are reference material, not a fallback authority.
- Use the installed Coordinator only for other repositories' governed runtime
  work and for ledger/decision operations. Never use it to test, install,
  deploy, roll back, or validate DevCoordinator2 itself.

## Skill and policy ownership

- This repository is the only writable source for these six skills:
  `dev-coordinator`, `formal-web-ui-verification`, `full-repo-audit`,
  `full-repo-test-coverage-audit`, `ui-implementation-audit`, and
  `user-journey-docs-audit`.
- `reference/universal/AGENTS.md` is the only universal policy source. Root
  `AGENTS.md` is repository policy and must not be installed globally.
- Installed skill and policy entries are direct absolute symlinks into this
  checkout. Preserve unrelated runtime entries and use the reviewed link
  managers under `scripts/skills/`.
- Shared policies, skill contracts, generated prompts, and generic
  documentation remain runtime-neutral. Runtime names belong only in factual
  installation adapters, provider metadata, product integrations, or history.
- Product Design audit and design-QA contracts are resolved through the active
  plugin catalog. Do not edit installed plugin copies.

## Skill development

- Reproduce a changed detector or workflow gap before editing it.
- Keep each `SKILL.md` authoritative and mirror enforceable behavior in its
  self-tests.
- Detector changes need realistic must-catch cases and false-positive guards.
- Keep `rust/tooling/` as the one shared source for audit builders, verifiers,
  evidence handling, and self-tests. Audit skills invoke the installed
  `devcoordinator2-tooling` binary; do not create vendored or standalone
  copies.
- Do not edit installed skill copies; change this repository and verify the
  direct links.

## Validation

- Run product lint/tests directly from this repository.
- Before complete skill validation, run the Rust workspace checks and build the
  release `devcoordinator2`, `devcoordinator2-tooling`, and
  `devcoordinator2-executor` binaries, including the tooling self-test fixture
  feature. The skill validator uses only the executor's `run-local`
  self-validation surface; it never self-hosts through the installed daemon.
- Run `devcoordinator2-tooling skills validate run` for the six-skill, policy,
  canonical ownership, Python-free, and browser matrix. It submits one strict
  schema-2 plan to the Rust executor; cheap policy, privacy, and ownership
  preflights invalidate expensive checks while unrelated siblings remain
  all-settled. Read bounded failures from its receipt and keep complete
  logs/report in the named `.devcoordinator/agent-validation/` run directory.
- The Formal Web UI self-test runs after the other skill self-tests because it
  measures a strict local response threshold; this is a concrete shared-host
  measurement conflict, not a general reason to serialize validation.
- Keep complete verbose output in cold logs and report bounded failure indexes.
- Finish finite diagnostic passes after ordinary failures, batch fixes, then
  rerun the complete relevant pass.
- Run the feature-gated `devcoordinator2-root-acceptance` binary only on Linux
  with explicit candidate daemon/executor paths and a new empty external work
  root. It owns one unique `devcoordinator2-rustint-*` systemd/Docker namespace,
  finishes all safe scenarios, and removes only marker-bound fixtures. Never
  aim it at the installed daemon or live state.

## Security boundary

- The live checkout is writable only by mutually trusted accounts controlled
  by the same owner. Its verified, commit-stamped Rust daemon binary executes as
  root when the daemon starts.
- Review `security-assumptions.md` before changing this trust model, adding a
  writer, or making repository writers mutually distrusting.
- Secrets, credentials, private instance values, and live evidence never enter
  source, prompts, normal results, indexes, or Coordinator-generated logs.
  Governed commands must not print them: their byte-complete stdout and stderr
  are private cold evidence disclosed only through explicit bounded log tools.
