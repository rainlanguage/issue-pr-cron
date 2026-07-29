---
description: The next ai:ready PR to rule on — the whole merge decision as one typed MCP result, cheapest-first.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_ready
---

Arguments: `$ARGUMENTS`

**LIMIT** is the whole argument if present — how many PRs to return. Omit it and
the tool defaults to 1.

Call `next_ready` **exactly once**, with `limit` set only if the caller gave
one, and pass what they gave verbatim. The range is the binary's to enforce, as
every other guard in this plugin is; relay its refusal rather than rounding a
number down, because a caller who asked for 10 wants a page this tool will not
honestly serve — each ruling changes the queue, so a page is stale past its
head.

That one call is the entire command. You have no other tool here, and that is
deliberate: a merge decision reassembled by hand from `gh` is a decision whose
inputs nobody can audit, and its shape drifts with whoever assembled it. If the
tool is unavailable, say so and stop — the answer is to connect the plugin's MCP
server, not to reach for `gh`.

## Why one call and not several reads

The queue is ranked cheapest-first by the same `presentable_queue` the vetter
and `queue` use, so the head of it already **is** the next decision — there is
no second ordering here to disagree with the first. Each field the result
carries is one read a human otherwise does by hand, in an order they have to
remember, and being wrong about any one of them changes the ruling:

- **the vetter's sha-bound verdict note** — the reasoning, not the label. A
  label says `ready`; the note says why. `verdict.sha` is the head that
  reasoning was written against and `verdict.atHead` says whether it still
  describes this code.
- **headRefOid and baseRefName** — not decoration. `rain-org-health` is on
  `master`, and assuming `main` has cost a run.
- **the CI rollup with failing checks named**, rather than counted. A
  `"rollup": "nochecks"` is not "all checks passed", and the empty
  `failingChecks` beside it is the assertion, not an omission.
- **whether CodeRabbit actually reviewed.** `codeRabbit.reviewed` is true for
  exactly one coverage value, `reviewed`. `rate-limited`, `queued`,
  `other-description`, `no-status`, and `unreadable` are all **not** coverage —
  a green check with no review behind it — and under any of them "0 unresolved
  threads" is **vacuous** rather than clean. The raw `checkState` and
  `description` are carried alongside so the misleading green is visible next to
  the truth about it.
- **unresolved threads**, whose `meaning` is already qualified by the field
  above.
- **the deploy-before-merge gate**, taken from the body and trusted producer
  comments — never the title, where the marker appears in 1 of the 6 PRs that
  carry it.

`queue.more` and `counts` frame the row: a PR you expected and did not get is
usually in `unvetted`, where a verdict that is no longer current at the PR's
head lands — a moved head un-pins the note from the code, and a `vet-protocol`
bump un-pins it from the rules it was written under.

## Present the result, do not summarise it away

Print every field. Then say plainly what it adds up to: whether anything blocks
a merge, and if the deploy gate is set, that this is deploy-before-merge and
**not** a plain merge — landing it as if it were ordinary is a production error.

Where the result is clean, say so and stop there. **This command does not merge
and does not rule.** It is the read that precedes the human's word; the writes
are `/reject`, `/design`, `/close-candidate`, `/keep-open`, and the merge itself
is the human's, on a PR they named.

Names collide across plugins; `/human-fsm:nr` disambiguates.
