---
description: The next ai:ready PR to rule on — the vetter's verdict, checked against an independent read of the diff and the issue it claims to close.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_ready, mcp__plugin_human-fsm_fsm__pr_context
---

Arguments: `$ARGUMENTS`

**LIMIT** is the whole argument if present — how many PRs to return. Omit it and
the tool defaults to 1.

This command is the human's **second opinion**, not a second copy of the
vetter's. A gate that reprints the upstream verdict in a table is that verdict
again, and it fails in exactly the case it exists for — the case where the
vetter was wrong. `rain.erc4626.words#230` is the live example: `/nr` presented
it as clean, at length, because the note said so, and the naming violation sat
in a file the vetter never opened and this command never opened either.

So the vetter's note is a **claim to check**, and checking it means reading the
diff and the issue yourself.

## The sequence

**1. `next_ready`** — which PR is next, and what the vetter concluded about it.
Call it with `limit` set only if the caller gave one, and pass what they gave
verbatim. The range is the binary's to enforce, as every other guard in this
plugin is; relay its refusal rather than rounding a number down, because a
caller who asked for 10 wants a page this tool will not honestly serve — each
ruling changes the queue, so a page is stale past its head.

**2. `pr_context`** on the PR that came back, addressed by the row's own `pr`
field, `owner/repo#n` — never a bare number, never a slug you reassembled. One
call per row if the caller asked for several. It carries the PR body, the
changed files with their additions and deletions, the full diff, every issue the
PR Closes or Refs with that issue's title, body and labels, and the trusted
`ai:vetter` / `ai:producer` comments. Step 1's `verdict.note` is clipped —
`noteTruncated` says when — and `vetterComments` here is not, so the vetter's
reasoning is read in full from this call.

**3. Derive the intent from the ISSUE**, and do it before reading the diff. Say
in your own words what a correct change would have to do, working from
`issues[].body` alone. Not from the vetter's note, and not from the PR body:
both were written by someone who already had an answer, and a description of a
diff is not a check on it. Where the PR links nothing — `closes` empty and
`issues` empty — that IS the finding. There is no independent standard to judge
it against, and saying so beats quietly promoting the PR's account of itself
into the standard it is measured by.

**4. Read the diff against that intent.** Three questions, each answered from
the hunks rather than from anyone's summary of them:

- does it do what the issue asked — all of it, or a subset, and which part is
  missing;
- does it do anything the issue did not ask — scope a reader would have to
  approve on its own merits, and which no `Closes` line covers;
- does anything in it look wrong on its own terms — a guard that does not guard,
  a test that would also pass on the unfixed code, a name or a convention the
  file around it contradicts.

Read the diff, not the file list. `#230`'s defect was in a file that appeared in
every summary of that PR and was opened by nobody. If `diffTruncated` is true
you did not get all of it: say which files you could not see, rather than
reading the omission as nothing to see.

**5. Put your read beside the vetter's, and say plainly where they diverge.**
Agreement reached independently is worth something; agreement by restatement is
worth nothing, and the reader cannot tell which one they were handed unless you
say. Where your reading **contradicts** the note, that is the headline and not a
footnote — an `ai:ready` that does not survive a second read is the single most
valuable thing this command produces, and burying it under the fields it
invalidates is how such a PR gets merged anyway. Where you agree, name what you
checked, so the agreement is a fact about the diff rather than a fact about the
note.

## These two tools and nothing else

Every input comes from a typed tool call. Do not reach for `gh`, do not assemble
a field by hand, and do not answer any part of it from memory: a merge decision
reassembled by hand is a decision whose inputs nobody can audit, and its shape
drifts with whoever assembled it. If a tool is unavailable, say so and stop —
the answer is to connect the plugin's MCP server, not to work around it. Both
grants are **reads**; this command has no write and needs none.

The analysis costs a second call and real reasoning, and that is the price of
the decision rather than an overhead to trim back out. It is additive to the
queue's latency, so a speedup elsewhere and this cost are one conversation and
not two — a faster queue that hands the human the vetter's own words back has
bought nothing.

## What `next_ready` has already settled

**Which** PR is not a second question. The queue is ranked cheapest-first by the
same `presentable_queue` that the vetter and `queue` use, so the head of it
already **is** the next decision — there is no second ordering here to disagree
with the first. And each field of the row is one read a human otherwise does by
hand, in an order they have to remember, where being wrong about any one of them
changes the ruling:

- **the vetter's sha-bound verdict note** — the reasoning, not the label. A
  label says `ready`; the note says why, and the why is what step 5 checks.
  `verdict.sha` is the head that reasoning was written against and
  `verdict.atHead` says whether it still describes this code.
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

Print every field of the row. Then give the independent read — what the issue
asked for, what the diff does, whether those two agree — and then say where that
read and the vetter's diverge. Then say what the whole of it adds up to: whether
anything blocks a merge, and if the deploy gate is set, that this is
deploy-before-merge and **not** a plain merge, because landing it as if it were
ordinary is a production error.

Clean is a conclusion you are allowed to reach, not one to reach for: say it
only about a diff you read against an issue you read, and say which those were.
**This command does not merge and does not rule.** It is the read that precedes
the human's word; the writes are `/reject`, `/design`, `/close-candidate`,
`/keep-open`, and the merge itself is the human's, on a PR they named.

Names collide across plugins; `/human-fsm:nr` disambiguates.
