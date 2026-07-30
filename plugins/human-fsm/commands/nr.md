---
description: The next ai:ready PR to rule on — the vetter's verdict, checked against an independent read of the diff, the issue it claims to close, and the audit skill run over the PR's own source at a declared pr:<number> scope.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_ready, mcp__plugin_human-fsm_fsm__pr_context, mcp__plugin_human-fsm_fsm__pr_checkout, mcp__plugin_human-fsm_fsm__clone_release, Skill, Read
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
diff and the issue yourself — and running the org's own review rules over the
source rather than recalling them.

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

**5. Run the `audit` skill over the PR's own source.** The mechanical half of
what makes code good is written down — in that skill — and a reviewer who
summarises a rulebook from memory is not applying it. Steps 3 and 4 come first
and are yours; this step is an instrument pointed at the same diff, and it runs
in this order so that the skill's report cannot become the frame your own
reading is written inside.

- **`pr_checkout`** on the same `owner/repo#n`, because the skill reads SOURCE.
  Cross-check the `head` it returns against `pr_context`'s `headRefOid` before
  reading a line: if they differ the tree is not this PR's, and every finding
  taken from it is about other code. **Never search for a checkout.** The `dir`
  in `pr_checkout`'s own result is the only path that is this PR's source — you
  do not glob for one, and a `vet-*` directory you happened to find is a
  DIFFERENT PR's tree.
- **Invoke the skill with the `Skill` tool** — `audit` — and DECLARE THE SCOPE
  AS AN ARGUMENT, not as something the reader is asked to remember:
  `scope=pr:<number>`, the `dir` the checkout returned, and the changed-file
  list. The skill's own top-line rule is _"whole-repo snapshot, never a diff —
  do not scope by recent changes / PR diff"_, so once it is loaded this file is
  not in the room and a scope carried only in prose loses to the document the
  invocation just pulled in. Measured on `rain.deploy#21`, it did: twelve
  findings, five bearing on the PR and seven in code the diff never touches,
  with the scope hand-typed as free text that nothing could check. The skill
  accepts exactly three scopes — `whole-repo`, `pr:<number>`,
  `paths:<comma-separated globs>`, the same vocabulary its run stamp records —
  and `/nr` always declares the second. A spelling outside those three is free
  text again.
- **Never hand-copy the skill's checks.** Invoking it is how this command
  inherits every upgrade to it, and the two findings that motivated this step
  were both stated plainly in it while a hand-rolled read missed them
  (`rain.deploy#20`, a newly added concrete test mock carrying a caret pragma;
  `rain.deploy#21`, a canonical CREATE2 derivation added and then hardcoded 22
  times beside 4 real calls). Run it INLINE and serial.
- **Every part of that argument comes from a typed result.** The `<number>` in
  `scope=pr:<number>` is the one inside the row's own `pr` field —
  `owner/repo#n`, the same string step 2 was addressed with — never a number
  read off a title, a URL or the vetter's note. The changed-file list is
  `pr_context`'s `files`, whole, each with its additions and deletions; if
  `filesTruncated` is true the list is a PAGE of `filesTotal`, and the scope you
  can honestly declare covers only what you were handed, so say which. The tree
  is `pr_checkout`'s `dir`. A scope you assembled yourself is the defect `#132`
  removed one level up: it looks exactly like a derived one, and nothing
  downstream can tell them apart.
- **What `pr:<number>` admits, and why — this is the REASON for the argument,
  not a second carrier of it.** In scope is the middle ground: the changed lines
  PLUS the code whose behaviour decides whether the diff is correct — the
  callees the changed lines invoke, the callers relying on the changed
  behaviour, sibling implementations sharing the invariant being changed, and
  every claim the PR body or the issue makes about how the code CURRENTLY
  behaves, since a stated current behaviour the source contradicts is a false
  premise and a finding in itself. NOT the diff alone; NOT a whole-repo audit on
  every `/nr`. The test for reading a file is "would understanding it change the
  ruling on THIS diff?".
- **Your read surface inside that tree is `Read`.** This harness has no `Grep`
  and no `Glob` — measured on 2.1.220, neither is listed and neither resolves
  through `ToolSearch` — so the lens navigates by path, from the changed-file
  list and the imports it finds there, and it does not search. Where a finding
  would need a repo-wide count, say what you counted and where, so the reader
  can see the bound on it.
- **Map its findings onto the ruling the human is about to make.** A defect in
  the diff's OWN changed code is a merge blocker and is named as one. A finding
  in code the PR does not touch is context, never a reason to withhold this
  merge. A question the diff raises and cannot settle itself is a `/design`
  rather than a quiet merge. And the skill finding nothing is not "clean": it
  never read the issue, so it cannot tell you the diff answered it.
- **Carry the declared scope through to what you present.** The value you passed
  is the one thing that says which code was read, so it is reported verbatim
  beside the findings rather than left to be inferred from them — a reader
  counting seven findings in untouched files should not have to work out that
  the lens swept the repo. If the skill's report contradicts the scope you
  declared, the scope is what the ruling is measured against and the divergence
  is the finding: say which one you got.

**6. `clone_release`** the checkout, passing the `dir` name `pr_checkout`
returned, before you present anything. A checkout left behind sits on the box
until a sweep reaches it — and this server has no `WORK_DIR`, so its clones land
in the temp-dir fallback, which the producer's sweep may never look in.
Unreleased checkouts are how this box filled its disk. If `pr_checkout` ERRORS
there is nothing to release and no audit lens either: re-call it ONCE, and if it
fails again present the read WITHOUT that half and say so in as many words — a
lens that never ran has no scope, and "no lens" is what you report, never a
scope you would have declared. A missing lens, named, is worth more than a lens
implied.

**7. Put your read beside the vetter's, and say plainly where they diverge.**
Agreement reached independently is worth something; agreement by restatement is
worth nothing, and the reader cannot tell which one they were handed unless you
say. Where your reading **contradicts** the note, that is the headline and not a
footnote — an `ai:ready` that does not survive a second read is the single most
valuable thing this command produces, and burying it under the fields it
invalidates is how such a PR gets merged anyway. Where you agree, name what you
checked, so the agreement is a fact about the diff rather than a fact about the
note.

## Why the skill does not make this a second vetter

The vetter runs the same skill, so the obvious worry is that step 5 duplicates
it and this gate stops being a second opinion. It does not, for three reasons
worth keeping straight.

**A shared rulebook is not a shared conclusion.** Two readers applying one
standard to one artefact are independent; a reader who paraphrases the other's
memo is not. What `#132` removed was the paraphrase — the verdict, relayed — and
step 5 adds no verdict: it adds the rules, while the conclusion is still derived
here, from the issue, the diff and the tree. Shared rules mean shared blind
spots, not shared answers, and a blind spot is fixed once in the skill and
inherited by both readers.

**The dimensions do not overlap.** The skill's subject is the code as it stands:
naming, storage class, pragma, derived constants, hazard surface. The scope
question — does this diff do what the issue asked, all of it, and nothing else —
is not a dimension it has, because it has no issue to compare against. That is
the half this gate has actually caught things with: `cyclo.site#393` closed an
issue with its boundary guard unbuilt, `#408` rendered four expired epochs while
closing the issue that complained about one, `#331` closed with its
`parseLeaderboardEntry` half undone. None of those is an audit finding, and
neither rain.deploy finding is a scope finding. Dropping either half loses a
class of defect that has already been landed.

**And the duplicate is hypothetical while the gap is measured.** The vetter
invoked the skill once across 35 verdicts in one run, and both rain.deploy PRs
reached `ai:ready` with the rules never applied to them at all. A check that
catches what upstream skipped is not redundant with it.

So: the skill supplies the rules, this command supplies the judgement, and step
5 is subordinate to steps 3 and 4 rather than a substitute for them.

## `whole-repo` is a different job, and it is asked for on purpose

A genuine whole-repo audit is a real thing to want — most obviously a repo you
are about to take a dependency on, before you depend on it. It is not this
command. `/nr` rules on ONE PR and its lens exists to decide THAT merge; a sweep
of every file answers a question nobody asked at this gate, and it answers it by
burying the findings that bear on the diff under the ones that do not — five
among twelve, on `rain.deploy#21`. So this command declares `scope=pr:<number>`
on every invocation and has no mode, flag or argument that declares anything
else: the whole argument is the LIMIT, and a scope is not something a caller
passes here.

That is deliberately not the same as removing whole-repo. It stays available as
a SEPARATE, explicit invocation of the same skill with `whole-repo` declared, on
a repo somebody named, outside this command — which is the only shape the skill
writes a `whole-repo` run stamp for anyway. What must never happen is a
whole-repo sweep arriving because no scope was passed. A scope that defaults to
the widest reading is indistinguishable from one that was chosen, and that is
exactly how this behaviour read as working for as long as it did.

## Typed reads and the lens, and no shell at all

Every input about the PR arrives from a typed tool call — the queue row, the
context, the tree. Do not reach for `gh`, do not assemble a field by hand, and
do not answer any part of it from memory: a merge decision reassembled by hand
is a decision whose inputs nobody can audit, and its shape drifts with whoever
assembled it. If a tool is unavailable, say so and stop — the answer is to
connect the plugin's MCP server, not to work around it.

The lens's SCOPE is one of those inputs and not an exception to the rule. It is
a value derived from two of those results — the PR ref the row named and the
file list `pr_context` returned — and passed as an argument the skill reads,
rather than a sentence about the PR written into an args string. Free text is
what nothing can check: the scope that produced twelve findings on a
five-finding question was typed out in full, correctly, and lost to the first
rule of the document it was handed to.

The grant is four typed calls plus `Skill` and `Read`, and `Read` applies to the
`pr_checkout` tree and nothing else. All four typed calls are reads except
`clone_release`, which disposes of what this command itself created and writes
no GitHub state. The rulings remain `/reject`, `/design`, `/close-candidate`,
`/keep-open`, and the merge is the human's, on a PR they named.

That list is a **declaration, not a sandbox**: measured on Claude Code 2.1.220,
a command granting only `Read` still ran a `Bash` call with no permission
denial. So the prohibition above is the thing that actually binds, which is why
it is written here rather than assumed of the frontmatter — and why nothing in
this file is fenced as a shell line, because what a reader copies out of a
command is what the command showed them.

The analysis costs three more calls, a skill fan-out and real reasoning, and
that is the price of the decision rather than an overhead to trim back out. It
is additive to the queue's latency, so a speedup elsewhere and this cost are one
conversation and not two — a faster queue that hands the human the vetter's own
words back has bought nothing.

## What `next_ready` has already settled

**Which** PR is not a second question. The queue is ranked cheapest-first by the
same `presentable_queue` that the vetter and `queue` use, so the head of it
already **is** the next decision — there is no second ordering here to disagree
with the first. And each field of the row is one read a human otherwise does by
hand, in an order they have to remember, where being wrong about any one of them
changes the ruling:

- **the vetter's sha-bound verdict note** — the reasoning, not the label. A
  label says `ready`; the note says why, and the why is what step 7 checks.
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
asked for, what the diff does, whether those two agree — then the audit lens's
findings, each marked as the diff's own code or as surrounding context, and then
say where the whole of that and the vetter's note diverge. Then say what it adds
up to: whether anything blocks a merge, and if the deploy gate is set, that this
is deploy-before-merge and **not** a plain merge, because landing it as if it
were ordinary is a production error.

**The lens's findings arrive under the scope they were formed at, stated.** Name
the value you declared — `scope=pr:<number>`, with the number in it — or, where
the checkout failed, that there was no lens at all. It is one line and it
decides how every finding under it should be read: a PR-scoped review and a
whole-repo sweep produce different lists, and a reader handed the list without
the scope has to reverse-engineer which one they got from the proportion of
findings that miss the diff. That is the inference this line removes.

Clean is a conclusion you are allowed to reach, not one to reach for: say it
only about a diff you read against an issue you read, with a lens you actually
pointed at the source, and say which those were. **This command does not merge
and does not rule.** It is the read that precedes the human's word.

Names collide across plugins; `/human-fsm:nr` disambiguates.
