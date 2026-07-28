---
description: Rule on an ai:close-candidate — uphold (rule, retire the flag, close) or reject the flag back to the producer.
argument-hint: <owner/repo#n> <uphold|reject> <note…>
allowed-tools: Bash(pr-review-report human-close:*), Bash(pr-review-report record-close-candidate-verdict:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **VERDICT** is the second word — `uphold` or `reject`.
- **NOTE** is everything after the verdict, and it is required.

Refuse and say why if SUBJECT is not `owner/repo#n`, if VERDICT is not one of
those two, or if NOTE is empty. **Never infer an owner or a repo.** One label
name covers two separately-sized populations —
`lanes.vetter-verdicts.ai:close-candidate` holds PRs, `closeCandidateIssues`
holds issues — so a bare number is a ruling on whichever population happens to
answer first.

Run **exactly one** command, and nothing else.

## `uphold` — the subject should be closed

```
pr-review-report human-close <slug> <n> <NOTE>
```

One transition, in a fail-safe order the binary owns: post the `👤 human` ruling
comment pinned to the head sha (a PR) or to the live producer flag (an issue),
apply `human:close-candidate`, retire the pending `ai:close-candidate`, then
close. It resolves PR-or-issue **by lookup**, so the reference alone decides
which population it acts on and neither you nor this command ever guesses.

On a subject that is already closed it clears a stale `ai:close-candidate` and
writes no ruling — the close is already on the record, and a reason dated today
would not be one.

## `reject` — the flag's evidence does not hold

```
pr-review-report record-close-candidate-verdict <slug> <n> reject <NOTE>
```

This drops `ai:close-candidate` and returns the issue to the producer's
uncovered queue, which may re-flag it on better evidence. It is **issue-only** —
a producer flag is a claim on an issue; on a PR the same label is the vetter's
own `close` verdict, and the tool will say so and name the moves that do apply.

If you mean "this must **never** be flagged again" rather than "not on this
evidence", that is the sacred ruling `/keep-open`, not this.

## After the command

Print its output verbatim: it names the provenance anchor and every label that
moved, which is the state you just put the subject in.

If it **refuses**, relay the refusal verbatim and stop. Every refusal here names
the moves that are legal from the state the subject is actually in — that
redirection is the answer, and it is one command each. Do not work around it
with `gh`: a raw `gh issue edit` or `gh issue close` is a transition outside the
state machine, and it is what left 74 closed subjects still carrying the flag.
