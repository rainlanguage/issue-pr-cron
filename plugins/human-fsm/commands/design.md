---
description: Rule design on a PR or an issue — record the answer and send it straight back to the producer, the same send-back a rejection is.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required: state the answer, not
  that there is one.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

A design ruling IS its answer, and the answer is producer work — the same
send-back a rejection is (#219). There is no parked spelling: a question still
open is already the `ai:design` state, so if you do not have the answer yet
there is nothing to rule — wait until you do. The work order is required,
exactly as it is on `/reject`:

```text
pr-review-report human-rule <slug> <n> design <NOTE> --rework <WORK ORDER>
```

Read the work order from the user's words: an instruction ("do X instead", "the
convention is Y — apply it") is the order, verbatim where possible. When the
note and the order are genuinely the same sentence, use it for both. When you
cannot tell what the producer should DO, ask — that one question is cheaper than
delegating a placeholder.

On a PR this lands `ai:reject` (every other `ai:*` cleared, `ai:design`
included) plus a `👤 human` comment pinned to the **head sha** recording the
ruling word `design`, plus the `Rework note @<sha>: …` work order in the exact
trusted form the producer's `trusted-comments --marker 'Rework note'`
verification accepts — one call, no raw `gh`, no marker to mistype. The producer
is the next mover from that moment; its push moves the head and the ruling goes
stale by itself.

**If it refuses with `is an ISSUE, not a pull request`**, run the command that
refusal names, with the same NOTE and the same WORK ORDER:

```text
pr-review-report human-rule-issue <slug> <n> design <NOTE> --rework <WORK ORDER>
```

On an issue a design ruling writes **no label at all**: the pinned
`Ruled …: design — <answer>` comment (and the work order beside it) is the whole
record, and the issue stays in the producer backlog to be worked per it.

On an issue carrying a **live** producer close-candidate flag this is refused on
purpose: the flag's own question must be answered first, and the refusal names
every legal move — pick one of those rather than working around it.

Any other refusal: relay it verbatim and stop. Do not reach for `gh`.

Print the command's output verbatim: it names the anchor, every label that
moved, and that the subject is DELEGATED to the producer — that last word is the
point.
