---
description: Rule human:keep-open on an issue — the close-candidate flag is wrong and this must never be re-flagged.
argument-hint: <owner/repo#n> <note…>
allowed-tools: Bash(pr-review-report human-rule-issue:*)
---

Arguments: `$ARGUMENTS`

- **SUBJECT** is the first word — an `owner/repo#n` reference. Split it on `#`
  into `<slug>` and `<n>`.
- **NOTE** is everything after it, and it is required.

Refuse and say why if SUBJECT is not `owner/repo#n` or NOTE is empty. **Never
infer an owner or a repo.**

Run:

```
pr-review-report human-rule-issue <slug> <n> keep-open <NOTE>
```

`keep-open` is **issue-only**: it answers a producer close-candidate flag with
"no", and a PR has no such flag to answer — `human:keep-open` on a PR is a label
the lane classifier buckets nowhere, so the ruling would record nothing at all.
The tool refuses it there and says so.

This is the one ruling that **clears** an `ai:*` label: `keep-open` contradicts
`ai:close-candidate` outright, so the flag goes with it. The comment pins to the
live flag's timestamp, exactly as the vetter's own verdict on that flag does, so
a re-flag would stale both records together.

It is the **sacred** answer — the issue is never re-flagged. If you only mean
"not on this evidence, the producer may try again", that is
`/close-candidate <ref> reject`.

Any refusal: relay it verbatim and stop. Do not reach for `gh`.

Print the command's output verbatim: it names the anchor and every label that
moved.
