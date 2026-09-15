# Security Assumptions

Last reviewed: 2026-09-07 (trusted local development and dependency-repair authority)

Installation-specific values (the concrete accounts, groups, domain, and
owner identity) are deliberately not in this file. They live in the
untracked `instance/` directory and in the installed instance configuration
(`docs/instance-configuration.md`).

## Confirmed local repository boundary

- A small set of local Unix accounts (enumerated in `instance/local-accounts.md`)
  is controlled by one owner and is intentionally not security-isolated
  within this repository.
- Every participating account requires equal full read, write, traversal,
  and Git-metadata access across the complete checkout, including `.git`.
- One shared Unix group (named in instance configuration) is the
  repository's single access mechanism. Access uses inherited group
  permissions and no named per-user ACL entries. New directories inherit the
  shared group and group `rwx`; new files inherit group read/write while
  preserving executable intent.
- No unrelated local users require access. World access is unnecessary. The
  edge service account is the sole named-ACL exception: it receives traverse
  on the checkout root and inherited read/traverse only on `edge/` and
  `console/`, never write access or Git metadata access
  (DC2-2026-09-01-EDGE-LIVE-SOURCE-READ).
- `/home/DevCoordinator2` is the one live source checkout. It stays on a clean
  `main` exactly fast-forwarded to `origin/main`; development mutations occur
  only in linked worktrees.

## Runtime trust boundary

- One owner controls all trusted local Unix accounts and coding agents.
  Local accounts are attribution and execution identities, not mutually
  distrusting tenants.
- Trusted local agents have standing owner authority to manage development
  configuration and diagnose, repair, verify, and install necessary
  DevCoordinator fixes that block already-approved non-production development.
  They do not need another conversational authorization for each defect or
  because its fix belongs to the Coordinator repository. Use canonical source,
  linked worktrees, direct self-validation, and the reviewed installer; preserve
  active work and verify the original affected surface. This authority does not
  extend to unrelated infrastructure, production changes, new public access,
  secret disclosure, or destructive persistent-data operations. Mandatory host
  and tool approvals remain binding. Re-review when ownership, environment, or
  these boundaries change (DC2-2026-09-07-TRUSTED-AGENT-REPAIR-AUTHORITY).
- Public Console users and Internet clients are untrusted until
  authenticated at the edge and granted access to specific deployments.
- The owner confirmed direct host loopback access to the Console is trusted
  for local development verification (DC2-20260915-LOCAL-IP-CONSOLE-ACCESS).
  The instance may explicitly enable `EDGE_TRUST_LOCAL_CONSOLE=1`. Only the
  kernel socket peer is used: IPv4 loopback, IPv6 loopback and mapped IPv4
  loopback qualify. Forwarded and cross-origin browser requests do not inherit
  this exception. The edge sends no public identity assertion for these
  requests, so existing local authority and actor attribution apply. No public
  user is impersonated and deployment-route grants remain unchanged. Review
  this exception if a local reverse proxy is introduced or host ownership changes.
- The owner explicitly confirms that an authenticated Console administrator
  has authority to execute every administrator command exposed by the
  Console. DevCoordinator does not add a second confirmation dialog or chat
  approval after authentication and server-side authorization. Destructive
  controls must still name their exact target and effect, and the server keeps
  exact-target validation, authorization, and permanent history. A host or
  tool-owned approval mechanism remains outside this Console rule.
- Repository source and permanent deployment/database data may be valuable.
  Test results and test scratch data are disposable and reproducible.
- Credentials, bot tokens, identity assertions, database passwords, and
  upstream secrets must not enter source, ordinary metadata, logs, metrics,
  or agent-facing results. They live only in instance configuration files
  outside the repository, systemd credentials, or private mode-0600 state.
- A repository declaration alone never authorizes an ignored Compose
  interpolation environment file. Private root-owned instance configuration
  must separately allow the deterministic repository identity and exact
  relative path. The owner gives trusted local agents standing authority to
  manage declared development environment-file grants within the existing
  same-owner, non-production checkout boundary, without a separate per-file
  conversational approval. The daemon validates the authorization, ignored-file
  state, and realpath on every use; it passes only the path to Compose and never copies
  values into configuration metadata, results, logs, metrics, or argv. This
  policy is loaded only by the root daemon; thin CLI/MCP clients and deployed
  application APIs receive neither its contents nor filesystem authority. This
  narrow exception does not permit committed credentials, symlinks/path
  escape, unrelated-account access, or production secrets, and must be
  re-reviewed when the repository trust boundary changes
  (DC2-2026-09-07-TRUSTED-DEVELOPMENT-CONFIG, superseding the per-file owner-review
  requirement in DC2-2026-08-28-COMPOSE-REPOSITORY-ENV).
- Existing trusted local callers and server administrators may grant/revoke
  one declared repository/file pair and reload the preconfigured policy live
  (DC2-2026-09-07-SCOPED-CONFIGURATION). This retains exact-target authorization,
  ignored-file/nofollow checks, ownership/mode, stale-revision detection, and
  value-free permanent receipts. Invalid reload retains the last good policy.
  Policy location and other instance settings are not live-editable. No
  previously refused repository grant is implied by installing this capability.
  The service may write its existing `/etc/devcoordinator2` directory to
  atomically replace that policy; `ProtectSystem=full` keeps the rest of `/etc`
  read-only (DC2-2026-09-07-POLICY-WRITE-SANDBOX). This does not add grants or
  change the policy location, owner, mode, or permitted callers.
- Trusted local agents may use those supported configuration operations as
  part of authorized development without asking the owner again. Exact grants
  remain auditable configuration, not additional conversational gates. This
  standing permission does not authorize credential disclosure, unintended
  public access, production changes, destructive data changes, or bypassing
  host/tool approval controls. Explicit later restrictions still apply
  (DC2-2026-09-07-TRUSTED-DEVELOPMENT-CONFIG).
- Runaway processes, containers, storage growth, stale work, malformed
  input, path escape, and lost replies are credible operational failures and
  are handled as such, not as security incidents.

## Glossary boundary

- The approved glossary interface reuses the existing Console roles and
  same-owner local boundary (DC2-2026-09-05-PROJECT-GLOSSARIES).
- Admitted Console users may read shared terminology. Project glossary reads
  require the existing repository viewing authority; private project terms
  and project-adoption enumeration do not become public shared metadata.
- Global Console administrators and trusted local callers may edit glossary
  concepts and guidance. No separate role, confirmation gate, external
  translation service or message-store access is introduced.
- Revision checks and inheritance validation protect editing correctness.
  They do not recast mutually trusted repository writers as hostile tenants.

## Governed test-log evidence

- The owner requires byte-complete stdout and stderr for governed checks and
  cases. The former per-stream storage cap is removed; storage growth is
  controlled by automatic completed-history retention, defaulting to 24 hours
  and the newest three runs for each repository/test/check/case identity
  (DC2-2026-09-02-PROGRESSIVE-TEST-LOGS).
- Complete streams are potentially sensitive owner-local cold evidence. They
  remain in caller-owned, mode-0600 repository state and are never included in
  ordinary status, completion, metrics, decision history, Console HTML, or
  model-facing results. The Coordinator does not claim to redact arbitrary
  subprocess output; governed commands remain responsible for not writing
  credentials or upstream secrets.
- Authenticated administrators and trusted local callers may discover this
  evidence through a content-free catalogue and explicitly request bounded
  portions of one exact run, check, case, and stream. Every lookup is
  repository-authorized and realpath-contained. Search is fixed-string by
  default, and no retrieval or failure summary invokes a language model.
- Structured diagnostics retain only validated bounded fields supplied through
  declared report formats or the inherited diagnostic channel. Unknown console
  text is not promoted into ordinary results. Source locations are
  repository-relative; log references are run-relative and never disclose a
  private absolute path through the public edge.

## Codex usage analytics boundary

- The owner explicitly authorizes the root daemon to read the content-free
  usage databases of every same-owner Unix account listed in a private,
  explicitly configured source policy and to combine those measurements by
  registered repository (DC2-2026-08-29-CODEX-USAGE-ACCESS).
- Public Console access to these combined measurements requires
  administrator access or an operator-or-higher grant on a deployment of the
  repository. Viewer grants are insufficient. Local Unix-socket callers keep
  the existing trusted local authority.
- Results never identify the contributing Unix account or Codex account and
  never return source paths, repository HMAC material, credentials, prompts,
  model output, source, commands, tool payloads, raw errors, thread or agent
  identifiers, or per-user values. Missing, incompatible, or unmapped
  collectors are disclosed as partial coverage rather than zero usage.
- Each Codex usage database remains the only writable accounting truth.
  DevCoordinator opens it read-only and stores only the privacy-preserving
  link between its registered repository and that collector's repository key
  (DC2-2026-08-29-CODEX-USAGE-SOURCE).
- The owner approved daemon-owned display caching on 2026-09-04. Rebuildable
  display projections may be retained in daemon memory under the same access
  and privacy boundary; collector databases remain read-only accounting truth.
  This approval does not add a durable database mirror, change source access,
  or authorize retaining prohibited values (DC2-2026-09-04-DAEMON-USAGE-SNAPSHOT-CACHE).

## Formal UI verification evidence

- Formal verification runs only against an in-scope safe local, fixture,
  preview, or explicitly authorized target.
- Each checked route, state, and viewport may retain an initial-viewport and a
  full-page screenshot. Callers mask sensitive regions explicitly.
- Reports never retain entered control values, action payloads, placeholder
  text, selected labels, credentials, or private source content. Privacy-safe
  control findings may retain control kind, selector, measured width,
  available width, and clipping amount.
- Review fingerprints contain only declared repository-relative UI input paths
  and SHA-256 digests. They never read outside the declared repository root or
  follow symlinked inputs.
- Manual-review state is caller-supplied and opt-in. The verifier never
  discovers a hidden latest baseline. Screenshot hashes prove artifact
  integrity but do not decide whether visual review repeats.
- A source/deployment binding is successful only when a value observed from
  the rendered deployment matches the declared source value. Missing or stale
  evidence remains a coverage failure.
- Governed formal-verification bundles live only inside the owning caller's
  private retained run leaf and expire with its test-log age/depth policy.
  A default-output formal run may use the leaf root; custom-output or concurrent
  runs publish unique immutable direct children below `formal-runs/`. The
  daemon enumerates only bounded SHA-256-named children, opens every component
  without following links, and confines each screenshot to its manifest's
  bundle directory.
  Public Console access is administrator-only, matching complete test logs;
  deployment viewers and operators cannot list screenshots or comments.
- The edge receives screenshot bytes only through bounded authenticated JSON
  requests. It never gains filesystem visibility into repository test state.
  The root daemon opens an exact repository/worktree/run/check/leaf/image with
  no-follow traversal, validates PNG metadata and the recorded SHA-256, and
  discloses at most 180 KiB of source bytes per response. Ordinary metadata
  contains no absolute path.
- Screenshot annotations are normalized geometry overlays in the authority
  database, never mutations of the evidence PNG. A top-level annotation is
  atomically linked to an ordinary Plan `user_feedback` task; replies and state
  changes retain attributable history. Only the author may edit their comment
  or explicitly delete their annotation, while every Console administrator may
  resolve or reopen the feedback under the existing administrator authority
  decision (DC2-2026-09-01-IMMEDIATE-ADMIN-ACTIONS,
  DC2-2026-09-02-VISUAL-EVIDENCE-ACCESS).

## Generic retained test evidence

- A direct process check that passed or ordinarily failed after confirmed
  cleanup may declare a bounded set of required,
  non-secret repository-relative evidence directories. The non-root executor
  snapshots only regular files without following links into the same private
  run leaf as its logs; empty, changed, oversized, special-file, overlapping,
  `.git`, and `.devcoordinator` sources fail the check. The manifest binds the
  copy to run, test, check, initial source digest, configuration digest, proof
  kind, and requested validation tier
  (DC2-2026-09-03-RETAINED-EVIDENCE-TREES). Failed-check diagnostic retention
  preserves those controls and never turns failure into passing build evidence
  (DC2-2026-09-05-FAILED-CHECK-EVIDENCE).
- These trees inherit the governed-log retention and disclosure boundary. A
  trusted local caller or authenticated Console administrator may list bounded
  path-free metadata and request one exact verified file chunk. Repository
  viewers and operators cannot read it. The CLI may materialize selected trees
  only into a new caller-owned local destination; the root daemon never writes
  an arbitrary export destination and never returns the private source or
  storage path.
- The Coordinator does not claim to detect arbitrary secrets inside declared
  files. Governed commands remain responsible for keeping credentials and
  upstream secrets out of retained evidence, as they already are for complete
  stdout, stderr, and structured reports.

## Owned event notification boundary

- The event journal follows the confirmed same-owner local and untrusted public
  edge boundary above. Trusted local callers may observe all typed events;
  public identities are reauthorized on every scheduler wake and receive only
  events for repositories or deployments visible through their current grants.
  Feedback and server-only events remain administrator-only.
- Event payloads are purpose-built typed projections, not copies of database
  rows or notification fields. They are capped at 8 KiB and omit credentials,
  e-mail addresses, raw logs, private paths, arbitrary subprocess text, and
  source content. The newest 1,024 entries are retained in schema 16; stale or
  future cursors fail explicitly instead of widening disclosure.
- A wait is passive observation. DevCoordinator returns matching events or
  elapsed heartbeat deadlines and never models agents, task execution,
  conversations, turns, wake-ups, or a server-initiated client action. Socket,
  HTTP, and MCP cancellation close the corresponding subscription.
- Schema-changing activation retains the pre-activation mode-0600 database
  backup. If activation fails after a schema change, rollback restores that
  backup before starting the prior binary; same-schema failures preserve the
  intact current database. This follows the confirmed valuable persistent-data
  and reviewed-restart assumptions (DC2-2026-09-03-VERIFIED-LIVE-RUST-BINARIES).

## Operating mode

- `devcoordinatord` (the DevCoordinator2 daemon) runs as root in a hardened
  systemd unit. It is the only component that changes system state.
- By explicit owner decision, the daemon, edge, and command-line source are
  loaded directly from the shared `/home/DevCoordinator2` checkout
  (DC2-2026-09-01-TRUSTED-LIVE-CHECKOUT). Every account allowed to write that
  checkout is controlled by the same owner and is trusted to affect code that
  executes as root on daemon restart. A dirty, stale, or non-`main` canonical
  checkout blocks restart and readiness.
- The non-root edge service receives read-only visibility of the canonical
  checkout and cannot write it. Other home content remains inaccessible to the
  edge except for the systemd path required to reach that source.
- Repository commands managed for other projects never run as root or as the
  daemon identity. The daemon launches them as the physical non-root caller via
  systemd transient units (`--uid`/`--gid` + explicit supplementary groups).
- Local API calls use a Unix socket. The kernel peer UID (`SO_PEERCRED`) is
  the physical caller identity; request bodies cannot assert identity.
  The socket is mode 0666: every local account is a full caller by owner
  decision (DC2-2026-08-24-OPEN-LOCAL-ACCESS); the client group remains
  only as the repository-access mechanism.
- Endpoint ownership uses a cooperative daemon lease acquired before state
  initialization, not a new caller authorization gate. A missing owned socket
  is rebound without restarting work, and cleanup preserves foreign replacement
  paths (DC2-2026-09-05-SOCKET-OWNERSHIP). The reviewed same-owner writers and
  open local access assumptions remain unchanged; this does not claim isolation
  from a malicious local account replacing the lease itself.
- No local per-repository or per-agent permissions are consulted. Any
  trusted local account may invoke any local command.
- Sandboxed trusted agents may be unable to create or connect any socket,
  including loopback sockets, while still writing bounded files in the host
  temporary filesystem. A daemon-owned sticky bridge directory may carry the
  same local protocol when the client observes `EPERM`; request-file ownership
  supplies the same trusted local UID/GID attribution, responses are private,
  and no network listener or public access is added. If local accounts become
  mutually distrusting, this bridge must be redesigned and reviewed.
- The public edge authenticates users and enforces per-deployment grants.
  Public authority never derives from the local trust boundary.
- Decision summarization is an append-only maintenance write over existing
  repository decisions. An authorized local caller or Console administrator
  may store a due summary directly without a separate user-approval step; the
  decision records and every prior summary remain permanent.

## Approved Rust control-plane cutover boundary

- The current Python daemon remains the live authority throughout development.
  The replacement is activated only after the single migration branch is
  merged into a clean `main`, every executable Python dependency is removed,
  and the complete frozen-candidate acceptance pass succeeds
  (DC2-2026-09-03-RUST-CONTROL-PLANE-V2).
- The installed daemon and local tools will execute locked Rust release
  binaries built directly from the canonical clean checkout. Each binary
  embeds the verified source commit. Before systemd or `/usr/local/bin` points
  to it, the installer compares that commit and the binary hashes with a
  root-owned installation manifest. A copied release source tree, a build from
  a dirty or non-main checkout, and an unrecorded replacement binary are
  refused (DC2-2026-09-03-VERIFIED-LIVE-RUST-BINARIES).
- Compiled artifacts do not change the existing trust decision: every account
  permitted to write the canonical checkout is controlled by the same owner
  and is already trusted to affect code executed as root after a reviewed
  restart. This does not add a writer, service, credential flow, network
  listener, or public authority. The edge remains non-root and read-only.
- The root daemon continues to launch governed repository commands out of
  process through systemd as the physical non-root caller. Linking the shared
  Rust protocol and evidence libraries into the daemon does not move a
  repository-authored command, environment, or parser into the root process.

## Docker: authoritative mode (since the 2026-08-25 cutover)

- No agent account is a member of the `docker` group
  (DC2-2026-08-22-DOCKER-MODE, executed 2026-08-25 on explicit owner
  approval). The root daemon is the Docker authority: unprivileged
  container work goes through DevCoordinator2's commands, and every
  container it creates is attributed to a repository, deployment or test,
  and caller.
- Containers that predate the cutover and were not created by the daemon
  remain visible and honestly classified (`observed-current` for adopted
  legacy stacks, `unmanaged` otherwise) until adopted through reviewed
  repository configuration or removed.
- Root/sudo held by the owner's accounts remains inside the single-owner
  trust boundary above; this section governs the unprivileged path, not
  root.

## Root daemon hardening obligations

Because the daemon is root and writes inside caller-writable repository
trees (`<repo>/.devcoordinator/`), it must:

- create and delete those paths with `O_NOFOLLOW`/dirfd-relative operations
  and refuse traversal outside the repository realpath;
- build every subprocess as an argv array, never a shell string; and
- treat all request content and repository configuration as untrusted input
  with strict validation.

## Review triggers

Review this file before: adding an account not controlled by the same
owner; making local accounts mutually distrusting; changing the repository
or client group model; placing credentials or private runtime state in the
checkout; exposing the daemon socket beyond the local trust boundary;
re-granting any account direct Docker access (reversing the authoritative
cutover); or exposing the repository through any network service.
Also review it before allowing another writer to `/home/DevCoordinator2`,
running the root daemon from a different checkout, or weakening the clean-main
fast-forward operating rule. Review it before broadening the edge ACL beyond
the two source trees or granting that service write access.
Also review it before exposing Codex usage to viewers, returning per-user or
raw collector detail, adding an exporter or network collector, supporting a
Codex usage schema or taxonomy beyond the explicitly reviewed versions, or
configuring a usage source owned outside the confirmed same-owner boundary.
Review it before exposing complete test streams outside the trusted local or
administrator boundary, retaining them outside caller-owned repository state,
adding non-deterministic or model-generated failure interpretation, or changing
the requirement that governed commands keep credentials out of their output.
