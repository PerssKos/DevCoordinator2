# Shared and Project Glossaries

## Ownership and purpose

DevCoordinator2 owns multilingual concept glossaries and terminology guidance.
Each project owns its localization architecture and exact messages, whether
they live in a database, JSON, text, framework resources or other files. The
glossary is not a translation platform or a message catalogue. It supplies
meaning and approved vocabulary to people and AI working on user-facing UI.

The database is the one editable authority. Console, CLI and MCP use the same
typed service. Permanent incremental revisions preserve changes without
maintaining independently editable source-file copies. Glossary revisions do
not replace decision history, completion tasks or governed execution evidence.

## Concepts, languages and guidance

A concept has a stable identity, readable name, definition, context, review
status, related concepts, and language-specific equivalents. Each language
entry contains a preferred term, allowed forms, deprecated alternatives,
usage notes, examples and a review flag. Review states are explicit owner
assertions, not automatic linguistic-quality certification.

Language tags are case-insensitive and use hyphen-separated primary language,
region, script or variant subtags, such as en, ru, pt-BR and zh-Hant. The
service validates bounded tag structure; it does not claim IANA registry
membership validation. Stored tag keys use lower case. Term comparisons
normalize canonical Unicode composition and surrounding whitespace, but do
not erase case or infer grammatical forms. Declare legitimate forms explicitly.

Concepts and named guidelines distinguish required rules, overridable defaults,
and advisory guidance. One word can represent different concepts in different
domains: a prohibition applies only to its identified concept and language.
User content, names, raw logs and quotations are not rewritten by this service.

The service never seeds invented domain definitions or translations. A new
glossary is honestly empty. Add and review the vocabulary needed by the project.

## Inheritance and adoption

The shared scope has no parent. A project has local concepts and guidelines
plus an explicitly adopted shared revision. An unconfigured project starts
with shared revision zero rather than silently adopting mutable global data.
The effective glossary reports its local revision, adopted shared revision,
current shared revision and whether adoption is outstanding.

Required shared concepts and guidelines cannot be shadowed. A default may be
specialized using the same identity (or guideline name) with an explicit
project reason. A different domain concept gets its own identity, not an
unexplained override. Required-rule conflicts reject adoption atomically.

Editing a shared concept creates a new shared revision. Existing project
baselines do not change until explicit adoption. Returning a specialization
to its shared concept preserves historical revisions. Changing a concept's
meaning clears the review flag for unchanged language equivalents; their
review can be recorded in a subsequent edit. No application messages change.

Every mutation requires the expected current scope revision. Stale edits fail
without partial writes. The Console preserves the draft and offers explicit
reload/discard recovery instead of overwriting another editor's work.

## Console journeys

- Glossary opens its concept collection first, with shared/project navigation,
  search across languages and aliases, and language/status/origin filters.
- Opening a concept shows its meaning first, then localized equivalents,
  permitted/deprecated forms, usage and related concepts.
- Add/edit opens a focused dialog in the current viewport. Language and
  related-concept changes remain drafts until save. Cancel restores focus.
- Saving a new concept reveals it in the collection. Saving an existing
  concept reloads its persisted detail through the API.
- Inherited entries link to their exact shared source revision. Required
  entries cannot be edited as project specializations.
- Guidance and languages manage glossary instructions, not translation files.
- History links to immutable scope revisions. Historical views are read-only.
- Project adoption shows administrators which baselines projects use. Shared
  updates are inspected and adopted deliberately, without changing UI messages.

Existing Console authorization applies: admitted users may read shared
glossaries; project reads require the existing project-viewing authority.
Only global Console administrators and trusted local callers may mutate.
Project adoption enumeration remains administrator-only. Archived project
glossaries remain readable but cannot be edited. These choices preserve the
confirmed same-owner and public Console boundaries in security-assumptions.md.

## CLI and MCP

Omit scope fields for the shared glossary. Supply JSON `path` for a local
checkout or `repository_id` for an existing project; never both. Public readers
use authorized repository identities rather than implicit registration.

```sh
devcoordinator2 operations --prefix glossary.
devcoordinator2 schema glossary.save
devcoordinator2 call glossary.list --params '{"query":"execution","language":"ru"}'
devcoordinator2 call glossary.resolve --params '{"path":"/absolute/project","limit":25}'
devcoordinator2 call glossary.get --params '{"concept_id":"CONCEPT_ID","repository_id":"REPOSITORY_ID"}'
devcoordinator2 call glossary.save --params-file /absolute/save-request.json
```

Each file contains the complete typed operation parameters, including scope and
`expected_revision`. Save wraps its concept in `concept`; configure and check
use their operation schema. JSON is the transport, not a requirement on a
project's localization storage. MCP uses `operation_schema` and `call_operation`
with the identical names and parameters.

List/resolve default to ten concepts, allow at most 25 per page and return
`next_offset`; a byte-bounded page may contain fewer than the requested count.
Pass the returned scope revision as `expected_revision` while
paging to reject mixed snapshots. History uses `next_before_revision`.
Historical list/get reads accept `revision`. Up to 24 languages per concept,
2,000 local concepts and 50 local guidelines are supported; limits fail
explicitly instead of dropping entries. Each concept is bounded to 24 KiB;
settings are bounded to 32 KiB and an effective glossary to 64 language tags.

Before UI work, an AI reads applicable project instructions and resolves the
relevant glossary, including inheritance and review state. It writes natural
messages through the project's own localization mechanism. Unknown concepts
or competing names are resolved explicitly. The targeted checker accepts only
approved concepts, reviewed equivalents and declared forms; outstanding shared
adoption prevents a valid result. It does not scan arbitrary prose, certify
translations, or claim that reading a glossary proves UI compliance.

## Verification

The domain tests cover persistence, schema upgrade, revision conflicts,
historical reads, explicit adoption, required-rule conflicts, specialization,
review invalidation, multilingual search, Unicode equivalence, legitimate
inflections, deprecated terms, cross-domain false-positive guards and archives.

`console/verify-glossary.mjs` drives the real Console through the real edge and
ControlPlane/SQLite service in an isolated feature-gated acceptance fixture.
It exercises CLI and MCP interoperability, role boundaries, long collections,
save/reload, cancel, validation, stale-edit recovery, inheritance, history,
guidance, and wide/narrow light/dark rendering. Synthetic concepts and accounts
exist only inside its disposable fixture, never in installed product data.
