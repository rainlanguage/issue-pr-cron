# Producer QA Guide — mandatory for every PR

Adversarial mutation testing of each PR is MANDATORY. A PR without its QA
evidence (section 8) does not get opened; a rework without it does not get
re-pushed. This guide exists because an entire evening of review rejected PR
after PR for the same defect: correct-looking fixes whose failure modes no test
could distinguish.

## 0. Understand before you act

The issue text is a CLAIM about the system, not a spec. Before classifying it
(fix / close-candidate / design question) or writing a line of code, derive what
the design actually IS from primary sources: the interface and base contracts,
the FRAMEWORK CALLER of the thing in question, and at least two sibling
implementations. Then judge the claim against that model:

- Claim contradicts the derived design and the design is coherent → the issue
  has an INVALID PREMISE → close-candidate with the derivation as evidence. Do
  NOT file it as a design question when your own evidence already answers it —
  deferring a judgment you have the material to make just moves your reading
  onto the human's plate.
- Claim exposes a real defect in the derived design → design question (the gate
  is for genuinely contested calls, not for unfinished reading).
- Claim is right and the fix is uncontested → implement, per the rest of this
  guide.

Canonical example: "integrity() ignores its declared-arity params" — reading the
framework's integrity loop shows it compares the RETURNED arity against the
declaration; per-word validation would duplicate one framework invariant across
every word. The convention answers the issue; that is a close, not a question.

## 1. Baseline

Run the suite green on the UNCHANGED code before touching anything. A red
baseline is its own bug to surface — never build on it, never mask it.

## 2. Discriminating tests — the core rule

Every behavior the diff claims to fix or add gets a test that:

- PASSES on the new code, and
- FAILS on the pre-change code (for a bug fix, the original bug IS the mutant:
  check out the base, run the new test, watch it fail — that run is your proof
  and it goes in the PR body).

"The suite is green" proves nothing by itself — a suite that passes on BOTH
sides of your change has pinned nothing (cyclo.site#398: three deploy-gate
fixes, 15KB test file untouched, every test green before AND after).

## 3. Mutation-validate the new tests

For each new test, apply ONE targeted mutation to the line it claims to cover
(negate the guard, flip the comparison, drop the call), confirm the test fails,
restore. A test that survives its own mutation is decoration.

## 4. Oracle discipline

- Expected values derive from the SPEC/ISSUE, never recomputed with the same
  function the implementation uses (mirror tests enshrine bugs).
- Fixtures must exercise the case the fix exists for: a decimals-split fix with
  all-18/18 fixtures makes every wrong usage an equivalent mutant
  (cyclo.site#372); README literals pin the mirror, not the source
  (erc4626#185).
- Symmetric properties cannot detect swaps — a*b == b*a whatever the order
  (flare#196's reciprocity). Prefer ASYMMETRIC invariants that fail under the
  exact confusion the issue names.

## 5. Guard strength

Nullish and type-safe: `?.` at every level that can be absent, `?? fallback` not
`|| fallback` (falsy-or passes non-bigint truthy garbage into formatters).
Recurring per-line patches mean a shared safe-accessor helper is the real fix.
(cyclo.site#389/#397.)

## 6. Coverage honesty

An issue's examples are illustrative, not exhaustive: cover the CATEGORY or link
`Refs` instead of `Closes`. Commit-message closing keywords must match the
intended close set exactly — they fire on merge regardless of the body.

## 7. Design gate

A test that would PIN contested behavior is a design question, not a test: post
the "awaiting human design ruling" comment and stop (revert-vs-floor,
erc4626#70, is the canonical case). Never introduce a second source of truth for
one fact (display strings beside bigint constants, split constants for a
definitionally-shared count) — ask "can these ever legitimately differ?" first.

## 8. QA evidence block — required in every PR body

```
## QA
- Discriminating tests: <test names> — each fails on base (<how verified>)
- Mutations applied: <line → mutation → killing test>
- Oracle: <where expected values come from, independent of the implementation>
- Category check: <issue asks A,B,C; covered A,B,C / Refs because ...>
```

The vetter rejects any PR whose body lacks this block or whose claims in it
don't hold.
