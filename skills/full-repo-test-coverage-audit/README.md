# Full Repo Test Coverage Audit

Explicitly invoke `$full-repo-test-coverage-audit` to assess complete executable
line/branch coverage, meaningful code and UI tests, and efficient focused test
execution. The repository stays read-only; audit artifacts live outside it.

The audit distinguishes completed review from meeting its requirements. Missing
runtime evidence or project timing budgets remains unproven. Structural test
references never imply execution or a coverage percentage.

```bash
devcoordinator2-tooling audit test-coverage build --repo /path/to/repo \
  --out /path/to/audit --coverage-report /path/to/coverage.info \
  --assurance-input /path/to/assurance.json

devcoordinator2-tooling audit test-coverage verify \
  --manifest /path/to/audit/manifest.json --reports /path/to/audit/reports
```

Evidence options are optional for initial gap discovery. The builder generates
an example input using the repository's actual source inventory. The
[evidence contract](references/evidence.md) describes native test reports,
source/configuration receipts, UI cells, project budgets and selector examples.

Workers inherit parent settings without effort overrides or effort bookkeeping.
One UI reviewer coordinates interaction and rendered evidence. Completed reports
and `review_ledger.json` establish review coverage; `assurance-report.json`
retains independent code/UI/efficiency findings and measurements.

Use `verify --require-assurance` when automation must require full assurance.
Exit codes are 0 for success, 1 for invalid audit evidence, 2 for setup failure,
and 3 for a valid audit with unmet or unproven assurance requirements. Legacy
artifacts remain readable without obsolete effort gates.

See [SKILL.md](SKILL.md) for the authoritative review workflow.
