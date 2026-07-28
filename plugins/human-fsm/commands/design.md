---
description: Rule human:design on a PR or an issue — it raises a design question a human must settle.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required: state the question, not
  that there is one.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

Run:

```
pr-review-report human-rule <slug> <n> design <NOTE>
```

That applies `human:design` and posts a `👤 human` comment pinned to the **head
sha**. It leaves every `ai:*` label alone.

**If it refuses with `is an ISSUE, not a pull request`**, run the command that
refusal names, with the same NOTE:

```
pr-review-report human-rule-issue <slug> <n> design <NOTE>
```

On an issue carrying a **live** producer close-candidate flag this is refused on
purpose: `human:design` there would strand the flag, since every AI transition
refuses once a human has ruled. The refusal names all four legal moves — pick
one of those rather than working around it.

Any other refusal: relay it verbatim and stop. Do not reach for `gh`.

Print the command's output verbatim: it names the anchor and every label that
moved.
