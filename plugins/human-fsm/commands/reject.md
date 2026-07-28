---
description: Rule human:reject on a PR or an issue — needs rework, with the reason on the record.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#` into
  `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required: on a PR it is the Rework note
  the producer will execute against.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never infer an
owner or a repo.**

Run:

```
pr-review-report human-rule <slug> <n> reject <NOTE>
```

That applies `human:reject` and posts a `👤 human` comment pinned to the **head sha**,
so a rework that moves the head makes the ruling visibly stop describing the code that
is there. It leaves every `ai:*` label alone — `reworked-reject` clears those once a
rework provably lands.

**If it refuses with `is an ISSUE, not a pull request`**, the refusal names the exact
command for the other subject. Run that one, with the same NOTE:

```
pr-review-report human-rule-issue <slug> <n> reject <NOTE>
```

The issue ruling pins to the issue as filed instead of a head sha. Note that on an
issue carrying a **live** producer close-candidate flag this is refused on purpose — a
`human:reject` there would strand the flag for ever, because every AI transition
refuses once a human has ruled. That refusal names all four legal moves.

Any other refusal: relay it verbatim and stop. Do not reach for `gh` — a hand-applied
`human:*` label binds to no anchor, records no reason, and is the failure this
transition exists to replace.

Print the command's output verbatim: it names the anchor and every label that moved.
