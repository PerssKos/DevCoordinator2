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

The repository-local commit hook uses the same Rust guard as CI:

```text
git config --local core.hooksPath .githooks
devcoordinator2-tooling check pre-commit --root /absolute/worktree
devcoordinator2-tooling check pre-commit --root /absolute/worktree --tree HEAD
```

Install the hook after building or installing the matching tooling binary. Preserve
any existing hook arrangement when configuring a checkout. The default command
reads staged blobs and their staged provenance sidecars, so an unstaged edit cannot
hide a staged secret or replace its evidence. CI checks the entire committed tree.
Both forms apply the existing privacy and public-artifact rules, reject generated
runtime evidence and invalid executable modes, and limit each new staged file to
10 MiB. The command reports paths and rule identities without printing file content.

An explicitly reviewed large file can be bound to its exact Git blob in the staged
`.devcoordinator-commit-allowlist.json`:

```json
{"version":1,"large_files":[{"path":"fixtures/reference.bin","blob":"<git hash-object identity>","reason":"Reviewed deterministic acceptance fixture"}]}
```

The exception applies only to that path and blob, never to privacy, provenance, or
executable checks. Replacing the file requires a new reviewed exception. The
allowlist itself must be a regular non-executable file no larger than 64 KiB.

The Linux root-acceptance fixture supports `--port-range START-END` (default
`31000-31999`) and `--compose-subnet PRIVATE-CIDR`. Choose fixture ranges that
do not overlap existing services or networks. Before starting any fixture, the
harness rejects privileged, malformed, reversed, or host-ephemeral-overlapping
port ranges. It does not change the host's ephemeral-port policy or remove
unrelated Docker networks when their default address pool is exhausted.

Pass the candidate paths explicitly with `--daemon`, `--executor`, and `--fixture`.
Build the daemon and harness together with the `root-acceptance` feature, then
copy all three executables into a new candidate directory before running the
harness. Later Cargo builds can otherwise replace a binary with another feature
configuration. Run only with `DEVCOORDINATOR2_ROOT_ACCEPTANCE=1` and a new empty
external `--work-root`; the harness never targets the installed service.
