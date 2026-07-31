---
description: Rule human:design on a PR or an issue — delegate the answer as a work order, or explicitly park the question.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required: state the question or
  the answer, not that there is one.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

A design ruling has TWO dispositions, and the choice is explicit — the tool
REFUSES a bare call rather than parking by accident:

- **The ruling answers the question and the answer is executable** → delegate.
  The producer works it per your order, and the label clears itself through the
  ordinary rework → un-vetted → re-vet flow:

  ```
  pr-review-report human-rule <slug> <n> design <NOTE> --rework <WORK ORDER>
  ```

- **The question genuinely stands** (you are raising it, or withholding the
  answer) → park, explicitly. Parked means parked: no AI actor touches the
  subject until you supersede your own ruling:

  ```
  pr-review-report human-rule <slug> <n> design <NOTE> --park
  ```

Read the user's intent from their words: an instruction ("do X instead", "the
convention is Y — apply it") is a delegation with that instruction as the work
order; a question with no answer yet is a park. When it is genuinely unclear
which they meant, ask — that one question is cheaper than parking a work order
or delegating a placeholder.

Either spelling applies `human:design` and posts a `👤 human` comment pinned to
the **head sha**; a delegation ALSO posts the `Rework note @<sha>: …` work
order in the exact trusted form the producer's
`trusted-comments --marker 'Rework note'` verification accepts — one call, no
raw `gh`, no marker to mistype. Both records pin the same sha, so they go stale
together when the producer pushes the rework.

**If it refuses with `is an ISSUE, not a pull request`**, run the command that
refusal names, with the same NOTE and the same disposition flag:

```
pr-review-report human-rule-issue <slug> <n> design <NOTE> --rework <WORK ORDER>
```

On an issue carrying a **live** producer close-candidate flag this is refused
on purpose: `human:design` there would strand the flag, since every AI
transition refuses once a human has ruled. The refusal names all four legal
moves — pick one of those rather than working around it.

Any other refusal: relay it verbatim and stop. Do not reach for `gh`.

Print the command's output verbatim: it names the anchor, every label that
moved, and whether the subject is DELEGATED or PARKED — that last word is the
point.
