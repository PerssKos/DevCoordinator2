## 12. Explain results through the user's goals and experience

- Explain work from the perspective of the user and their requirements,
  not from the implementation's internal structure. Start with what the
  user wants to accomplish and how the result helps them accomplish it.
- Describe what people can now do, what they could not do before, what
  remains incomplete, and how those facts affect their intended use.
  For operational work, explain the observable effect on the system or
  workflow they rely on.
- Do not substitute jargon, acronyms, component names, or lists of
  technical changes for an explanation. Naming a mechanism does not
  explain its purpose or consequence.
- When a technical concept matters, explain it in the user's context
  before using its technical name. For example, explain “who can see or
  change accounts” rather than merely naming a permissions acronym.
- Before requesting a decision, make clear what the user is choosing,
  why their input is needed, how the choices differ in actual use,
  and which choice best satisfies their requirements.
- Explain verification through the behavior it demonstrates, not only
  through commands, test names, or pass counts.
- Keep explanations proportional and useful. Do not replace jargon with
  lengthy background lectures, repetitive helper text, condescending
  analogies, or hypothetical warnings. Put optional technical detail
  after the user-facing account.
- Report meaningful preliminary results promptly, with clear access
  instructions and limitations. Distinguish progress from final readiness
  without making user acknowledgement a condition for continued work.
- Distinguish facts, inferences, assumptions, and genuine blockers.
  Do not claim fixed, ready, complete, or done while the intended outcome,
  required verification, or request-related completion work remains open.

### Minimum explanation contract

Every agent-authored progress update, blocker report, test result, decision
request, handoff, and completion report must answer these three questions in
plain language, in this order:

1. **Result:** What can the user or operator do now, or what remains
   unavailable?
2. **Meaning:** Why does that result matter, and what exactly was verified,
   partially verified, or left unverified?
3. **Next step:** What concrete action comes next, who or what must perform it,
   and what condition will unblock or complete the work? For completed work,
   say that no further action is needed instead of inventing a follow-up.

Use normally two to four sentences, or a short Result / Meaning / Next step
list when that is easier to scan. Brevity must not remove the consequence or
recovery path. Do not report only an internal state label such as “stopped at
guard,” “source mismatch,” “blocked,” “pending,” or “timeout”; explain what
that state means for the user's task in the same paragraph.

Technical names, run IDs, component names, status codes, and test counts are
supporting evidence. Put them after the plain-language explanation and never
use them as its substitute. Raw logs and machine receipts may remain technical
when retained as cold evidence.

Use these patterns:

- **Successful work:** “You can now … . I verified …, so … works for the
  requested journey. No further action is needed.”
- **Partial verification:** “The change is present, but … has not been
  verified. This means … remains uncertain. Next, … must be run against … .”
- **External blocker:** “The requested action cannot be completed because … .
  The affected result is … . … must provide or repair … before work can
  continue.”
- **Stale deployment:** “The check did not run because the review site is
  serving an older version than the current checkout. The change is therefore
  unverified there; update the review deployment and rerun the check.”
