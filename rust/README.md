# Rust execution plane

This workspace contains the Rust control plane, reusable governed-check
executor, and shared tooling. Use the repository's pinned Rust toolchain.
The executor accepts plan schema 2 only.

Build the release binary from the canonical checkout:

```text
cargo build --release --locked -p devcoordinator2-executor
```

Run or validate a JSON/TOML plan:

```text
cargo run -p devcoordinator2-executor -- run PLAN.json
cargo run -p devcoordinator2-executor -- run-local PLAN.json
cargo run -p devcoordinator2-executor -- validate PLAN.json
```

`run` requires the host broker in `DEVCOORDINATOR_CAPACITY_SOCKET` and fails
closed when it is absent. Explicit `run-local` is the direct self-validation
mode and admits all dependency-ready leaves locally.
The control plane pre-creates disposable `current_dir` and stable `log_dir`;
the executor resolves all plan paths before writing and refuses a missing or
escaping run directory. This
accepts platform path aliases such as macOS `/var` → `/private/var` without
weakening containment.
Repository commands remain argv arrays; the executor never invokes a shell.

The control plane can request bounded content-free evidence without
retaining a second hashing implementation:

```text
devcoordinator2-executor source-digest --worktree /absolute/worktree
devcoordinator2-executor receipts-match --worktree /absolute/worktree --receipts receipts.json
```

Normal commands print one bounded JSON receipt. Every direct check, discovery
step, and expanded case writes byte-complete private streams and indexes below
the exact stable `log_dir` named by the plan. There is no aggregate output copy
or per-stream storage cap. `log-query` and `log-prune` are JSON-stdin internal
surfaces used by the authenticated control plane; they expose bounded logical
references rather than caller-supplied paths.

After an isolated CI skill-validation run, export bounded diagnostics with:

```text
devcoordinator2-tooling skills validate evidence --root /absolute/ci-checkout --output /new/diagnostics-directory
```

The export includes typed report summaries and the last 16 KiB of each exact
stdout/stderr stream, including executor diagnostics when no final report was
sealed. It does not traverse fixture or evidence directories, copy authentication
state, follow links, or overwrite an existing export. Known credential and private
identity lines are withheld using the public-artifact guard; the command is for
isolated CI fixtures, not arbitrary live runtime logs. CI uploads only these JSON
files and retains them for seven days. Complete local streams remain private.
