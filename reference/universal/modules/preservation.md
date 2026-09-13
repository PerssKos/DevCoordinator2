## 11. Protect canonical sources, data, and running systems

- Before creating large generated trees or artifacts, inspect the relevant
  filesystem's capacity and resolve approved storage placement from the
  coordinator's host and project decisions. Prefer a designated bulk volume
  for latency-insensitive archives, release staging, large snapshots, and
  inactive worktree material. Reserve memory-backed temporary filesystems for
  small, short-lived scratch; free space on another mount does not enlarge them.
- Keep latency-sensitive hot work on appropriate storage. Do not globally
  redirect temporary files, relocate canonical source, or move active builds
  and services merely to use a larger disk. Preserve ownership, credentials,
  permissions, compatibility paths, and the project's coordination controls.
- Retire completed scratch work deliberately. Before removing a worktree,
  refresh its Git baseline, distinguish merged or equivalent changes from
  unique commits and dirty work, and check active process, deployment, and
  nested-repository references. Preserve useful unfinished work and rollback
  identities; remove only verified obsolete artifacts with exact-target
  receipts. An old name or timestamp alone is not proof of disposability.
- Treat canonical sources as the only writable truth. Update installed,
  generated, mirrored, or derived copies through their verified source
  workflow.
- Before broad audits, refactors, migrations, history changes, or repository
  splits, establish the checkout's relationship to the current remote.
  Remote-unavailable means unknown.
- Never discard, hide, stash, reset, or rewrite valuable dirty work for a
  clean base. Preserve it and reconcile through an evidence-backed merge
  from a verified baseline.
- Before mutating a running service, shared resource, or persistent store,
  inspect its state and use applicable coordination, locking, backup, and
  recovery. Preserve failure evidence before restarting and verify recovery
  through the same surface.
- Before destructive data work, verify a recoverable backup or prove the
  target disposable and isolated.
- Tests that create persistent state isolate or safely clean up their own
  state, respect dependencies and concurrent runs, and never
  unconditionally delete shared records.
- Use explicit working directories and unambiguous mutation targets.
  Verify the intended result before reporting success.
- Model data by domain meaning, ownership, lifecycle, reuse, validation,
  and evidence needs. Shared transport or presentation does not imply
  shared ownership. Separate concepts that change for different reasons
  and name their contents truthfully.
