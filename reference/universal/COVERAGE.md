# Focused universal policy: manual coverage map

Baseline: `760dcf3`, `reference/universal/AGENTS.md` (893 lines). The numbered
sections were moved intact into the following modules, except for the explicitly
approved timing, selection, polling, and review revisions listed below. This is
a source mapping and integration contract, not a completion ledger. Outcomes,
decisions, and execution evidence remain in their authoritative services.

| Baseline section | Detailed destination | Preserved requirement families |
| --- | --- | --- |
| 1 (3–72) | `modules/outcomes.md` | Infer complete scope; necessary foundations; independent progress; interim steering; real-surface bug repair; explicit limits, pauses, blockers, and final completion |
| 2 (73–106) | `modules/context-evidence.md` | Applicable requirements and older corrections; negative acceptance criteria; bounded context and cold logs; catalogue-first evidence; untrusted external content; authoritative third-party facts |
| 3 (107–189) | `modules/approval-security.md` | Every questions/approval rule; proportional security-assumptions gate; actual authority; concise material questions; no blanket hardening or scope expansion |
| 4 (190–230) | `modules/ledger-decisions.md` | Database authority; decisions/options/cost/risk/supersession; rolling summaries; unfinished outcomes versus executions; diagnosed tasks; readable outcomes; evidence and readiness |
| 5 (231–276) | `modules/execution.md` | Dependency concurrency; asynchronous ownership; delegation interfaces and limits; host capacity; failure dependencies; events, cursors, bounded fallback watcher, and observed completion |
| 6 (277–426) | `modules/delivery.md` | Eligible clocks and defaults; accessible web previews; every desktop target and real updater; concurrent incremental publication; original verification/access boundaries; recovery-only hard stop |
| 7 (427–469) | `modules/verification.md` | Manual prose review only; end-to-end-first test selection; existing-test extension before new unit coverage; focused versus stable/full validation; frozen candidates; safe sealed runs; isolated repairs; one suite owner; realistic journeys, recall, and precision |
| 8 (470–496) | `modules/truthful-results.md` | Real facts/data/persistence/errors; enabled controls do their work; truthful prototypes/disabled future UI; complete end-to-end results |
| 9 (497–699) | `modules/ui-design-gate.md`, `modules/user-interface.md` | Journey-led destinations; exactly three design alternatives; approval/autonomy; mockup-backed fidelity audit; compact contextual controls; row preservation; no engineering commentary; purposeful text; minimal surfaces; all rendered interactions; glossary and content-first states |
| 10 (700–753) | `modules/corrections.md` | Confirmed mistakes versus changed intent; original-surface diagnosis; existing outcomes and standing corrections; batched prevention/fix; applicability and provenance; durable discoverability; no writable legacy ledgers |
| 11 (754–796) | `modules/preservation.md` | Storage placement/capacity; hot versus bulk work; exact cleanup with preservation; canonical sources; verified remote baseline; dirty work; running services; recoverable data; isolated tests; domain ownership |
| 12 (797–827) | `modules/communication.md` | User goals and observable results; meaningful choices; behavior-based evidence; proportional explanation; preliminary access; truthful gaps and completion |
| 13 (828–893) | `modules/performance-review.md` | Daily continuity; original specification; per-task attributable resources; no double counts or invented allocations; evidenced causes; scoped improvements; reviewed dependency repair; optional gaps; user report and durable references |

## Approved revisions reviewed manually

- `AGENTS.md` is the mandatory core, not a replacement for selected details.
  `modules.json` v1 preserves section order. Always-applicable scope, evidence,
  approval/security, truthfulness, communication, and review rules cannot be
  excluded. Conditional rules are loaded before their actual work; uncertainty
  includes details. Nothing promotes universal policy above user/project rules.
- Section 5 replaces the universal 100 ms ceiling with events or one
  service-owned, cancellable, deadline-bounded backoff watcher. It does not
  restore model-turn polling or create another capacity/scheduling authority.
- Section 6 follows `APP-WIDE-TWO-STAGE-DELIVERY-OVERRIDES` and
  `APP-WIDE-PROJECT-DELIVERY-DEADLINES`: performance-only work has no delivery
  alarms; an authorized implementation target establishes its baseline; at
  24 hours one request runs concurrently; only at 36 hours does the affected
  scope become recovery-only. Explicit user revisions preserve actual times,
  retire obsolete wakes, and reevaluate blocks. Targets remain independent.
- Persistent clocks/wakes/admission belong to the agent runtime. Coordinator
  retains outcomes, evidence, decisions, and execution capacity. Review and
  delivery receipts remain separate. No API or scheduler backend is added here.
- Section 13 extends daily review with deduplicated bottleneck review and the
  hypothesis → options → chosen action → measurement → keep/revert protocol.
  Speed to useful results comes first without reducing required quality.
  Waiting for the user and intentional required release revalidation are not
  waste. Automatic action stays in the reviewed repository and current scope;
  specification work does not authorize product implementation. Existing
  separately authorized dependency-repair rules still require their own review.
- Section 7 makes end-to-end acceptance evidence the first test level for a
  changed behavior. Existing end-to-end or integration tests are extended
  before new scenarios, and existing tests are searched and extended before
  any new unit test. Unit coverage remains supporting evidence for isolated
  logic and cannot replace a missing end-to-end path.
- Mockup-backed UI completion now requires a combined `$product-design:audit`
  against the confirmed visual target, paired source/rendered evidence at the
  same state and viewport, and repeat passes until no P0-P2 finding remains.
  `ui-design-gate` owns the detail; Section 5 separates admission from handoff
  and Section 9 preserves rendered interaction proof. P3 polish is documented
  follow-up. No confirmed target means no fidelity gate; an unavailable approved
  target blocks it. Approved references remain usable, while new audit runs
  need fresh implementation captures. `FORMAL-UI-REVIEW-TRIGGER` remains in
  force: affected UI inputs, intent, targets, or route/state/theme/viewport
  changes trigger review; dynamic pixels and unrelated backend work do not.
  The gate supplements interaction and formal-browser evidence and permits
  clearly incomplete preliminary previews.
- All web/desktop delivery, updater, security, UI, preservation, and verification
  requirements remain conditional on their original relevant work. The new
  performance-only gate supersedes the former all-project delivery trigger,
  not those delivery acceptance criteria.

Manual review follows `APP-WIDE-AGENTS-PROSE-NO-TEST-RUNS`: no tests or automated
policy-validation suite are used to validate this wording. Executable loader
tests use synthetic instruction text, not assertions against this prose.
