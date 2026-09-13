# Codex focused-policy integration

## Parent-owned hook

The current Codex checkout renamed `project_doc` to `agents_md`. This change
adds a clone-only host-instruction filter to `LoadedAgentsMd` and focused-policy
context helpers; existing legacy discovery remains unchanged. Session wiring,
matcher registration, and scheduler integration are parent-owned. The
crate-visible entrypoint is the existing exported type's associated method:

```rust
let legacy = AgentsMdState::add_focused_policy(&mut world_state, &policy_file, &applicability, step_context.loaded_agents_md.as_deref()).await?;
world_state.add_section(legacy.unwrap_or_else(|| AgentsMdState::new(step_context.loaded_agents_md.as_deref())));
```

Insert the first call immediately before the existing AGENTS section in
`codex-rs/core/src/session/world_state.rs`. No additional export or `lib.rs`
edit is required. `policy_file` is the configured host-local universal
`AGENTS.md`, normally `config.codex_home.join("AGENTS.md")`; it is not inferred
from the project name, a remote executor path, or model-provided URL. The
loader follows that installation symlink to the canonical bundle directory.
Installing the split policy must preserve access to that entire directory,
not copy the compact core alone. Parent owns installation/activation.
Non-Codex consumers without focused selection read every manifest-listed
module from the resolved canonical directory; they retain the complete policy.

Also register `super::world_state::AgentsMdState::matches_focused_policy` in
`CONTEXTUAL_USER_FRAGMENT_MATCHERS` in
`codex-rs/core/src/context/contextual_user_message.rs`. This parent-owned
integration keeps the typed policy fragments classified as context rather than
user-authored messages. No existing AGENTS marker or precedence is changed.

Pass the current bound purpose as lowercase `discussion`, `specification`,
`analysis`, `implementation`, or `recovery`. Default autobinding is `analysis`
with performance-only scheduling. Additional tags describe actual planned/tool
work: `planning`, `research`, `documentation`, `diagnosis`, `testing`, `review`,
`feedback`, `delegation`, `operations`, `data`, `security`, `ui`, `web`,
`desktop`, or `delivery`. `performance-only` is also recognized. Purpose adds
its necessary detail tags: discussion → planning; specification → planning
and documentation; analysis → planning, diagnosis and review; implementation
→ planning and testing; recovery → implementation, operations, diagnosis,
testing and delivery. Manifest arrays are an ordered any-of match, not a
permission grant or a scheduling obligation.

Binding through `project_automation` must precede changes. Refresh applicability
after a purpose binding or action/tool applicability change and before the
next affected work/model request. The runtime—not this text loader—owns this
ordering and the clocks. Tool names are not guessed from prompts: map actual
actions to the relevant tags, or include an unknown tag to load all details.
A purpose-only classification cannot exclude a domain that remains uncertain.
Empty/unknown work tags select all modules; unknown manifest selectors retain
that module. Never downgrade a load error into successful core-only loading.

## Context and compatibility

Only a file beginning `<!-- codex:focused-policy:v1 -->` opts in. Unsupported
focused-policy versions fail explicitly. An unmarked full AGENTS file, even
one larger than the focused budget or beside an unrelated `modules.json`,
continues through the unchanged legacy loader. The mandatory core is always
included in focused context. The loader returns the verified source and exact
core bytes with its selected chunks. Only when the host instruction source
canonicalizes to that same source and both whole strings compare equal after
`.trim()` does a cloned legacy state omit the host instruction. This approved,
parent-owned comparison normalizes only outer whitespace, matching
`CodexHomeUserInstructionsProvider::load_from_codex_home`; it requires no
raw-preservation hook. Internal whitespace and all other content must match
exactly. Prefix or substring matches never qualify, and neither stored string
is rewritten by the comparison.

Project and internal entries, even identical text, retain their order and
provenance. Additional custom content, internal-whitespace differences,
different or unresolved canonical source paths, and unmarked full AGENTS files
remain untouched. The original loaded object and old model history never
change. Core-only deduplication emits no empty legacy instruction fragment;
an incremental removal notice keeps the separate focused policy in effect.
Module applicability, selection order, and verified core content are unchanged.

The entrypoint adds 32 stable bounded world-state slots. Selected content is
loaded atomically, in manifest order, then emitted as typed user fragments
under `core/context`. Content-derived revision equality suppresses unchanged
updates. Changed selection/policy appends one revision; subsequent requests
are quiet. Removed parts receive explicit removal notices. Retained-history
checks restore required parts after compaction without rewriting history.
Each part carries its revision and index. Only the first part includes the
short revision/precedence notice; continuation parts do not repeat it.
The notice supersedes only focused-policy revisions, never direct
user instructions or more-specific project instructions. Keep the legacy
AGENTS section last among these user-instruction sections.

V1 limits: 32 modules, 16 applicability tags per module, 48-byte lowercase
ASCII/digit/hyphen identifiers, 256-byte local relative paths, 16 KiB manifest,
32 KiB per file, and 120 KiB selected text total. Module paths start `modules/`;
absolute paths, empty/dot/parent components, backslashes, colons, NULs,
duplicates, and symlink escapes are rejected. Reads are capped and require
regular UTF-8 files. Required missing/invalid/oversized content is an error,
not silent truncation. There is no network fetch or credential access.
These controls implement the explicit safe-local-path/bounded-context request
within the confirmed same-owner source boundary in both projects'
`security-assumptions.md`; they do not claim hostile concurrent-writer isolation.

Each separate fragment holds at most 4,096 body bytes plus an envelope below
512 bytes. This is below the 10K-token item ceiling even at one token per byte.
**P0 manual context review:** a new fragment can exceed 1,000 tokens. Review
the wrappers, precedence, ordered UTF-8 splitting, unknown fallback and total
budget at integration. Do not concatenate the fragments into an unbounded
single message. Scheduler static context is separate from these slots.

## Verification ownership

Focused synthetic tests exercise the actual `WorldState` entrypoint, native
purpose/action changes, unknown fallback, update-once behavior, retained
history, ordered chunking, removals, full-AGENTS compatibility, invalid schema,
missing/oversized content, relative-path rejection and symlink containment.
Dedup tests additionally cover canonical-source identity and whole-content
comparison with outer-whitespace normalization, including the parent-tested
positive host-trimmed-core case. Same-path custom content, other/unresolved
paths, untouched project/internal entries and
provenance, immutable old history, one-time removal, revision mismatches,
canonical-equivalent symlinks, and unchanged full-AGENTS behavior remain covered.
Run `just test -p codex-core --lib -E 'test(focused_policy) or test(context::world_state::agents_md)'`
for these and the adjacent existing context snapshot. Parent owns integrated agent-tool
coverage, global formatting/schemas and the scheduler acceptance fixture in
`acceptance/project-alpha.json`. No scheduler behavior is claimed by loader
tests, and no prose-policy suite is required.
