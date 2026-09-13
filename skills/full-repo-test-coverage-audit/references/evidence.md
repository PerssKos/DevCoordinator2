# Coverage and efficiency evidence

The audit reads existing test evidence. It does not run commands from input
files. Paths in the assurance input are relative to that input's directory;
paths in a run receipt are relative to the receipt. Absolute artifact paths
are supported. Source/test paths are repository-relative. Artifacts must be
regular files, not symlinks, and every declared SHA-256 is checked again at
verification. JSON objects reject duplicate keys and unsupported schema fields.

## Assurance input (schema_version 1)

Pass this document with `build --assurance-input <path>`. The generated
`assurance-input.example.json` includes the actual source inventory and the
required review topics. Fill it from confirmed requirements, source inspection
and real runner artifacts, then rebuild the audit with it. The example is not
completed evidence. Input changes require rebuilding to bind the new bytes.

Fields:

| Field | Meaning |
| --- | --- |
| `scope` | One `{file, role, rationale, reference}` per manifest source. Role is `product`, `tests`, `support` or `excluded`. Product code needs full runtime measurements. Other roles need a concrete reason and requirement/decision/source reference. |
| `checks` | Categorized execution graph described below. |
| `runs` | Artifact references to run receipts emitted by the existing test workflow. |
| `source_dependencies` | Mapping of a source path to its direct dependencies, including fixtures and shared configuration. Review completeness against the build/import graph. |
| `ui_requirements` | Required interaction cells described below. |
| `ui_exclusions` | Exact discovered target ID to `{rationale, reference}` for a control that is demonstrably outside rendered behavior. |
| `selection_cases` | Actual selector observations for local, shared, UI, fixture, config and unmapped changes. |
| `selection_exemptions` | Map a non-applicable case kind to its concrete rationale. Do not exempt a difficult or unimplemented selector. |
| `budgets` | `{reference, focused_ms, pre_merge_ms, release_ms}` with positive project-approved wall-time limits. No universal defaults are invented. |
| `reviews` | Evidence-backed judgments described below. |

An artifact reference is `{ "path": "reports/result.json", "sha256": "<64 hex characters>" }`.

A check has:

```json
{
  "id": "profile-component",
  "category": "component",
  "tier": "development",
  "variant": "chromium/light/desktop",
  "inputs": ["src/profile/**", "tests/profile/**"],
  "requires": ["build-fixtures"],
  "dependency_reasons": {"build-fixtures": "Consumes the generated test fixtures"},
  "resources": [],
  "repeat_reason": null
}
```

Categories: `unit`, `component`, `contract`, `integration`, `e2e`, `visual`,
`static`, `setup`. Tiers: `development`, `pre-merge`, `release`. `inputs` uses
repository-relative globs. Every dependency requires its actual correctness or
artifact reason. Resources name genuine shared conflicts, not invented capacity
limits. `variant` identifies a real browser/platform/features configuration;
it must not be changed just to hide duplicate execution. `repeat_reason` is
only for intentional repeated proof of the same behavior and configuration.

Review entries are `{criterion, status, rationale, references}`. Status is
`met`, `gaps`, `unproven` or `not_applicable`; references are a nonempty list of
source, requirement, decision or run-evidence references. Required criteria are
`assertions`, `duplication`, `parallelism`, `setup`, `waiting`, `retries`,
`layering`, `selection`, plus `ui_inventory` and `ui_assertions` for a rendered
UI. Review evidence must explain why a test's assertions detect the advertised
failure and why overlapping tests or serialized work are justified. An
unexplained label cannot establish that judgment. Automatic findings remain
visible even if a review says `met`.

## Native reports and source-bound run receipts

The project's existing runner emits the receipt **with the run**, binding the
source/configuration captured before execution and the final native reports.
Do not hash today's source and attach it to yesterday's result. If a historical
report lacks this provenance, retain it as structural/context evidence and
leave runtime assurance unproven.

A run receipt has these fields:

```json
{
  "schema_version": 1,
  "run_id": "project-owned-run-id",
  "source_hashes": {"src/profile.ts": "<sha256>", "tests/profile.test.ts": "<sha256>", "package.json": "<sha256>"},
  "config_hashes": {"package.json": "<sha256>"},
  "environment": "project runner and browser/toolchain identity",
  "captured_at": "2026-09-10T12:00:00Z",
  "tier": "release",
  "complete": true,
  "collection_complete": true,
  "branches_enabled": true,
  "status": "passed",
  "duration_ms": 1200,
  "coverage_reports": [{"path": "coverage.info", "sha256": "<sha256>"}],
  "supporting_artifacts": [],
  "checks": [{
    "check_id": "profile-component",
    "start_ms": 100,
    "duration_ms": 1000,
    "status": "passed",
    "phase_ms": {"setup": 100, "execution": 800, "cleanup": 100},
    "reports": [{
      "artifact": {"path": "junit.xml", "sha256": "<sha256>"},
      "format": "junit",
      "source_file": "tests/profile.test.ts"
    }]
  }]
}
```

This is a shape example, not measured data. `source_hashes` must exactly cover
all manifest sources, including tests and configuration. `config_hashes` binds
the actual relevant runner/build settings. `start_ms` is relative to the run;
duration and optional phase measurements must be real and nonnegative. A full
release run with complete collection and passing executed tests is required
for numerical coverage assurance. Selected runs can provide focused feedback
and UI interaction evidence but cannot masquerade as a complete release run.

Native report formats:

- `junit`: testcases, optional files/times, failures/errors/skips. Names include
  the producer's `classname::name` when a classname is supplied. Use
  `source_file` only when the producer lacks file metadata.
- `playwright-json`: recursively reads suites/specs and expanded cases;
  identities include their suite/title/project, outcomes and retry attempts.
- `rust-list`: libtest `--list --format terse`; identities remain `collected`
  and never count as executed. `source_file` is required.
- `rust-json`: native libtest JSON event lines. `source_file` is required.
- `collected-tests`: a portable adapter output with
  `{schema_version: 1, tests: [{file, name, status, duration_ms, attempts}]}`.
  Use an existing runner's deterministic adapter for other frameworks; never
  fill a successful result by hand. It also represents expanded parameterized
  names. Status is `passed`, `failed`, `skipped` or `collected`.

A test reference is its exact `file#name`. Unambiguous supported source
registrations also support structural evidence, including Rust `#[test]`
functions inside ordinary source files. Dynamic/unsupported declarations need
native collection. Listing tests, retrying until green, and asserting unrelated
constants cannot establish meaningful behavior coverage.

Coverage formats retain independent line and branch measurements. Missing branch
metadata differs from a measured zero. Raw artifacts are reparsed so editing
normalized manifest counts cannot manufacture coverage. For a file covered by
several partial reports, supply the coverage tool's correctly merged full report;
the audit does not union ambiguous branch counts across producers or runs.
Review instrumentation scope and exclusions, including runner-level omissions.

## Required UI cells

Each `ui_requirements` row supplies:

```json
{
  "id": "profile-save-light-desktop",
  "file": "src/profile.tsx",
  "target_ids": ["<exact target ID from the manifest>"],
  "journey": "save profile",
  "state": "editing",
  "theme": "light",
  "viewport": "desktop",
  "interaction": "Save",
  "expected": "The saved values survive reload",
  "test": "tests/profile.spec.ts#profile > saves changes > [chromium]",
  "check_id": "profile-browser",
  "reference": "requirements/profile.md#save-profile",
  "formal": null
}
```

Bind every discovered control to at least one cell; use the requirement-derived
inventory for dynamic surfaces the scanner misses. One journey can cover several
controls. Required themes/viewports must have distinct collected test or check
identities. The UI reviewer validates the actual assertions, cancellation,
failure/recovery, persistence and inventory completeness. Do not infer complete
coverage from a test's title.

For existing formal web evidence, set `formal` to
`{report, review_queue, manual_review, cell_key}`. The first three are artifact
references; `cell_key` is the returned `reviewCellKey`. The existing formal
review validator checks hashes, screenshot integrity and exact review coverage;
the selected cell must pass and match the required state and viewport. Its page
must be checked, match the declared theme, have a matched deployment source
binding, and have no blocking findings. Include the formal report in the same
interaction run receipt's `supporting_artifacts` to establish freshness. This
supports a real interaction test and does not replace it. Do not recreate formal
reports or review documents manually.

## Focused selection evidence

Each `selection_cases` row is `{kind, observation, additional_checks_reason}`.
The optional reason justifies extra smoke checks or other purposeful expansion.
An observation is emitted by the project's selector/listing adapter:

```json
{
  "schema_version": 1,
  "base_revision": "verified-comparison-base",
  "source_hashes": {"src/profile.ts": "<sha256>", "tests/profile.test.ts": "<sha256>", "package.json": "<sha256>"},
  "changed_files": ["src/profile.ts"],
  "selected_checks": ["build-fixtures", "profile-component"]
}
```

The audit follows changed source dependencies and check prerequisites. It reports
omitted required checks and unjustified extra checks; unknown mappings require
broad selection. Confirm the dependency map independently against source/build
metadata and demonstrate a known affected failure. This evaluates the existing
selector, not a new selection or scheduling service.
