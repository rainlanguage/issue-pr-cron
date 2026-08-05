---
description:
  Rule reject on a PR or an issue — a send-back, with the reason AND the work
  order on the record in one call.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule:*), Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required. It carries TWO records
  the tool keeps distinct: the RULING (why this is rejected — provenance, pinned
  forever) and the WORK ORDER (what the producer must do — spent once executed).
  Split the note accordingly: the reason stays the ruling note, the actionable
  instruction goes in `--rework`. When the user gave one sentence that is both,
  use it for both rather than inventing content.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

Run:

```text
pr-review-report human-rule <slug> <n> reject <RULING NOTE> --rework <WORK ORDER>
```

On a PR that applies **`ai:reject`** — the ONE reject state, whoever ruled — and
posts TWO comments pinned to the **head sha**: the `👤 human` ruling (authority:
it is what makes this a human's ruling rather than the vetter's, and what the
vetter reads when it re-judges the rework) and the `Rework note @<sha>: …` work
order, in the exact trusted form the producer's
`trusted-comments --marker 'Rework note'` verification accepts. One call does
both — there is no second `gh` command, and a mistyped marker is impossible.

`--rework` is REQUIRED on a reject: a reject IS a send-back, so there is no
parked spelling here. If the rework is not worth doing at all, that is a
different ruling — `/design` with `--park` for an open question,
`/close-candidate` for a decided close, `human-close` to close now — and the
tool's refusal names them.

Two things follow:

- **The PR is the producer's while the ruling is at head.** The order is its
  work order; no AI actor records a verdict over the current ruling.
- **The producer's push is the transition.** The head moves, ruling and order go
  stale together (they pin the same sha), and the PR re-enters vetting from
  scratch — with your note in front of the vetter. Nothing has to be called
  after a rework.

The ruling also clears any stale `ai:*` verdict, so the PR lands in exactly one
state.

**If it refuses with `is an ISSUE, not a pull request`**, the refusal names the
exact command for the other subject. Run that one, with the same NOTE and the
same `--rework`:

```text
pr-review-report human-rule-issue <slug> <n> reject <RULING NOTE> --rework <WORK ORDER>
```

The issue ruling writes `human:reject` — an issue has no vetter-side reject for
it to be one half of — and pins to the issue as filed instead of a head sha; the
work order rides with it the same way. Note that on an issue carrying a **live**
producer close-candidate flag this is refused on purpose, because a
`human:reject` there would strand the flag for ever. That refusal names all four
legal moves.

Any other refusal: relay it verbatim and stop. Do not reach for `gh` — a
hand-applied label binds to no anchor, records no reason, and a hand-typed
rework note is the silent-park bug this transition exists to remove.

Print the command's output verbatim: it names the anchor, every label that
moved, and whether the subject is DELEGATED — that last word is the point.
