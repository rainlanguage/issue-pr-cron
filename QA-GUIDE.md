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

Green on UNCHANGED code first; a red baseline is its own bug to surface.

## 2. Discriminating tests — the core rule

Every behavior the diff claims to fix or add gets a test that:

- PASSES on the new code, and
- FAILS on the pre-change code (for a bug fix, the original bug IS the mutant:
  check out the base, run the new test, watch it fail — that run is your proof
  and it goes in the PR body).

"The suite is green" proves nothing by itself — a suite that passes on BOTH
sides of your change has pinned nothing (cyclo.site#398: three deploy-gate
fixes, 15KB test file untouched, every test green before AND after).

## 3. Mutation-validate the new tests — with the bundled tool

The route is the `adversarial-mutation-test` skill scoped to your change, whose
probe step authors ONE targeted mutation per behavior (its catalog) into a
`mutants.toml` and runs the bundled bin:

```sh
nix run github:rainlanguage/adversarial-mutation-test#mutation-probe -- mutants.toml
```

`mutation-probe --help` is the manual: file format, verdicts, exit codes. Do NOT
hand-roll an edit-run-restore loop — the bin ENFORCES what a hand loop can only
assert. A red, silent or zero-test baseline aborts before any probe; the suite's
own tally proves it RAN, so a crash or compile error is NO-RUN and never a pass;
each target must occur EXACTLY once in its file; and every restore is verified
byte-exact before the next mutant. A test that survives its own mutation is
decoration.

## 4. Oracle discipline

The skill's adversarial pass owns the method; these are the local precedents.
All-18/18 decimals fixtures make every wrong usage an equivalent mutant
(cyclo.site#372); README literals pin the mirror, not the source (erc4626#185);
symmetric properties cannot detect swaps (flare#196's reciprocity).

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

All four lines are required. A line your change cannot have takes `n/a` **with
the reason** (a docs-only diff has no mutations to apply); an absent line is not
an option. `Mutations applied` is TRANSCRIBED from the probe's verdicts (§3) —
the mutant, and the test that KILLED it — never recalled; a mutation nothing
scored is not evidence.

That is enforced where the PR is opened, not only where it is judged: `open_pr`
reads the body file and REFUSES (exit 3) — PRESENCE only — before anything is
created, so "a PR without its QA evidence does not get opened" is literal.

The gate cannot reach a PR already open without the block. The retrofit is
`pr-review-report repair-qa-block`, whose `--help` is the manual. A body edit
moves no commit, so push an `--allow-empty` commit afterwards or the vetter
skips the PR as already vetted at that head.
