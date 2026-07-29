---
description:
  Rule reject on a PR or an issue — needs rework, with the reason on the record.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required: on a PR it is the Rework
  note the producer will execute against.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

Run:

```
pr-review-report human-rule <slug> <n> reject <NOTE>
```

On a PR that applies **`ai:reject`** — the ONE reject state, whoever ruled — and
posts a `👤 human` comment pinned to the **head sha**. There is no
`human:reject` any more: a reject means the producer reworks, which was always
true of both labels, so the state names the work and the pinned comment names
the ruler.

That comment is not decoration, it is the **authority**. It is what makes the
ruling a human's rather than the vetter's, it is what the vetter reads when it
judges the rework, and it is the reason the PR is parked while the head still
matches it. The vetter cannot write one — every comment it can post begins with
its own marker.

Two things follow, and they are why the note has to be worth executing:

- **The PR is parked while the ruling is at head.** No AI actor records a
  verdict or flags a state over it.
- **A push un-parks it, with no further transition.** The head moves, the ruling
  stops describing the code, and the PR re-enters vetting from scratch — with
  your note in front of the vetter. (`reworked-reject` is gone; nothing has to
  be called after a rework.)

The ruling also clears any stale `ai:*` verdict, so the PR lands in exactly one
state.

**If it refuses with `is an ISSUE, not a pull request`**, the refusal names the
exact command for the other subject. Run that one, with the same NOTE:

```
pr-review-report human-rule-issue <slug> <n> reject <NOTE>
```

The issue ruling is unchanged: it writes `human:reject` — an issue has no
vetter-side reject for it to be one half of — and pins to the issue as filed
instead of a head sha. Note that on an issue carrying a **live** producer
close-candidate flag this is refused on purpose, because a `human:reject` there
would strand the flag for ever. That refusal names all four legal moves.

Any other refusal: relay it verbatim and stop. Do not reach for `gh` — a
hand-applied label binds to no anchor, records no reason, and is the failure
this transition exists to replace.

Print the command's output verbatim: it names the anchor and every label that
moved.
