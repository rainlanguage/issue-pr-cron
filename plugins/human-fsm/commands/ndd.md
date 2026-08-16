---
description: The next ai:design PR to rule on — the raised design question checked against the issue, the diff and the code it is about, then presented with its code-constrained option space where the answer is the human's, or ruled here where it is already answered or misrouted.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_design, mcp__plugin_human-fsm_fsm__pr_context, mcp__plugin_human-fsm_fsm__pr_checkout, mcp__plugin_human-fsm_fsm__clone_release, mcp__plugin_human-fsm_fsm__human_rule, Skill, Read
---

Arguments: `$ARGUMENTS`

**LIMIT** is the whole argument if present — how many PRs to return. Omit it and
the tool defaults to 1.

This command is the human's **second opinion on a question**, not a reprint of
it. A gate that relayed the raised question verbatim would be that question
again, and it would fail in exactly the case it exists for — the case where the
question should never have been asked. `/nr` learned the general lesson on
`rain.erc4626.words#230`, presented as clean at length because the vetter's note
said so; nothing about that failure is specific to verdicts. A design question
is upstream text like any other: whoever raised it had already decided the
answer was not theirs to give, and THAT decision is the thing being checked
here, before the question itself is.

So the raised question is a **claim to check** — the claim that a human decision
is genuinely required — and checking it means reading the issue, the diff, and
the code yourself.

## What is different from `/nr` and `/ncc`, and it is the core

`/nr` checks a verdict against a diff; `/ncc` falsifies a one-line reason. Here
the upstream text is a QUESTION, and a question can be defective in ways a
verdict cannot: it can be real, it can be already answered, and it can be no
question at all. Those are three different findings with three different
deliverables, and the whole job is telling them apart:

- **A genuine human question** — the code admits more than one defensible design
  and picking one is a judgement call, not a lookup. The deliverable is the
  question presented with the minimal context needed to answer it, the option
  space **as the code actually constrains it** (which options the tree in front
  of you forecloses, which it leaves open, and what each costs), and a
  recommendation. The human's answer is the exit.
- **Already answered** — something already settles it, and the pointer IS the
  finding: name where the answer lives, and rule that answer. Do not re-derive
  it as if it were open; a settled question re-asked is how precedents drift.

  **What this command can actually reach, and what it cannot.** Two sources are
  verifiable here: a ruling or a statement in the PR's own linked issues and
  trusted comments (`pr_context` carries them), and a convention the code states
  — the pragma of the file next door, the sibling implementation, the constant
  already shared — which `Read` reaches inside the checkout. A ruling recorded
  on **any other issue or PR** is reachable by no tool in this grant: there is
  no issue search here, no `gh`, and no fetch.

  So if you recall such a precedent, say so **as a recollection, marked
  unverified**, name the issue or PR you believe carries it, and let the human
  confirm it — never present it as a checked pointer. That is the same move step
  4 makes for a truncated diff and step 5 makes for a failed checkout: name the
  read you could not make. It does not license answering from memory, which the
  grant section below forbids for exactly this reason; it makes the memory
  visible AS memory instead of laundering it into a finding.
- **Not a design question** — the flag is misrouted: the question is askable of
  the code (a read would answer it), answerable by precedent the raiser did not
  consult, or resting on a false premise about what the code currently does. The
  finding is the misroute, stated with **what the correct state would have
  been** — a verdict the vetter could have recorded, producer work that needed
  no permission, or a needs-work with the defect named — and that correction is
  what you rule.

Name the finding before you argue it. A presentation that drifts between the
three is the reprint this command exists to replace.

**The report is as long as the finding needs.** A genuine question earns the
full presentation, because a human has to answer it. The other two are rulings
you take here, and their report is the pointer or the correction, the ruling
taken, and the work order it carries — a few lines.

What shrinks is the write-up, never the read. The finding is only reachable BY
deriving the intent from the issue, reading the diff against it, and testing the
question's premise, and which of the three you are holding is knowable only
afterwards. A run that started guessing which questions are real would fail in
exactly the case this command is worth something in.

## The sequence

**1. `next_design`** — which question is next, and who raised it. Call it with
`limit` set only if the caller gave one, and pass what they gave verbatim. The
range is the binary's to enforce, as every other guard in this plugin is; relay
its refusal rather than rounding a number down — a caller who asked for 10 wants
a page this tool will not honestly serve, because every ruling retires its row
and a page is stale past its head. The queue is **oldest question first**: the
label parks the PR outside every AI actor's queue, so the wait is the cost, and
FIFO bounds it.

**2. `pr_context`** on the PR that came back, addressed by the row's own `pr`
field, `owner/repo#n` — never a bare number, never a slug you reassembled. One
call per row if the caller asked for several. It carries the PR body, the
changed files, the full diff, every issue the PR Closes or Refs with that
issue's title, body and labels, and the trusted comments unclipped — step 1's
`question.note` is clipped, `noteTruncated` says when, and this call is where
the whole raising comment is read. `question.note`'s share is the PAGE's, so a
`noteTruncated` on a multi-row page has a cheaper escape first: re-ask step 1
at `limit: 1`, which carries several times what the widest page can.

**3. Derive the intent from the ISSUE, and do it before weighing the question.**
Say in your own words what the issue asked for, from `issues[].body` alone — not
from the question, and not from the PR body: both were written by someone who
had already framed the problem, and a question inherits its framing. Most
misroutes are visible from here alone: a question the issue already answers, or
a question about scope the issue never asked for. Where the PR links nothing,
that IS a finding to report alongside the question — a design decision with no
stated requirement behind it is being made against nothing.

**4. Read the diff and the question against that intent.** Three questions,
answered from the hunks rather than from anyone's summary of them:

- is the question REAL in this code — does the tree actually admit the
  alternatives the question implies, or has the diff already foreclosed all but
  one;
- what does each option COST here — which files move, which invariants bend,
  which conventions the surrounding code states would each choice keep or break;
- does the question's premise HOLD — a question that asserts "the code currently
  does X" where the source shows otherwise is a false premise, and a false
  premise is a misroute finding, not a question to relay.

If `diffTruncated` is true you did not get all of it: say which files you could
not see, rather than reading the omission as nothing to see.

**5. Where the question turns on code, check out the code.** A design question
worth a human's answer usually turns on what the tree actually permits, and the
diff alone cannot show the callees, the siblings sharing the invariant, or the
convention the neighbouring files state.

- **`pr_checkout`** on the same `owner/repo#n`. Cross-check the `head` it
  returns against `pr_context`'s `headRefOid` before reading a line: if they
  differ the tree is not this PR's. **Never search for a checkout** — the `dir`
  in `pr_checkout`'s own result is the only path that is this PR's source.
- **Your read surface inside that tree is `Read`**, navigating by path from the
  changed-file list and the imports it names — this harness has no `Grep` and no
  `Glob`. Read what decides the option space: the callees the changed lines
  invoke, sibling implementations sharing the invariant the question is about,
  and every file whose convention an option would have to follow or break.
- **The `audit` skill is the same instrument `/nr` carries**, for the same
  reason: where weighing the options means judging code quality, the mechanical
  half of that judgement is written down, and a reviewer summarising the
  rulebook from memory is not applying it. If you invoke it, invoke it with the
  `Skill` tool and DECLARE THE SCOPE AS AN ARGUMENT — the literal `pr:<number>`,
  the number from the row's own `pr` field, with the checkout's `dir` and
  `pr_context`'s file list beside it — and report its findings under that
  declared scope, each marked as bearing on the question or as context. The
  skill's whole scope vocabulary is `whole-repo`, `pr:<number>` and
  `paths:<globs>`; this command declares the middle one and has no spelling for
  the others. It is an instrument, not a step: a lens invoked because the
  sibling command invokes one is ceremony, and `/ncc` is the standing example
  that omitting it can be the argued choice.
- **`clone_release`** the checkout, passing the `dir` name `pr_checkout`
  returned, before you present anything. This server has no `WORK_DIR`, so its
  clones land in the temp-dir fallback no sweep may reach; unreleased checkouts
  are how this box filled its disk. If `pr_checkout` errors, re-call it ONCE; if
  it fails again, present the read without the tree and say so in as many words
  — a checkout that never existed needs no release, and a missing read, named,
  is worth more than one implied.

**6. Put your read beside the raised question and say plainly where they
diverge.** Agreement reached independently is worth something; agreement by
restatement is worth nothing, and the reader cannot tell which they were handed
unless you say. A question that does not survive a second read — already
answered, or no question at all — is the most valuable thing this command
produces, and burying it under a faithful reprint of the question is how a
settled matter costs a human decision anyway. Where that is what you have, TAKE
the ruling per **If you can articulate it** below and report that you took it: a
question you answered and handed on is a question still queued.

## If you can articulate it, send it BACK — not forward

Answering IS routing: the answer lands the PR back with the producer as
`ai:needs-work` with the answer as the trusted work order, posted in the same
call at the same anchor. There is no parked spelling and no waiting state — a
design ruling without an executable answer is not a ruling yet, and the machine
holds no state for half of one.

**An answer you can put into words is a send-back.** Rule it with `human_rule`;
the words you were about to write for the human ARE the work order, and they go
in `rework`. Both verbs land the one `ai:needs-work` state — `design` where the
note answers the question raised, `needs-work` where the correct move was work
with a defect named — so the verb records which one ruled rather than deciding
where the PR goes.

Two of the three findings exit here:

- an **already-answered** question — rule that answer, with the pointer in the
  note;
- a **misroute** — rule the correction, so the producer executes the correct
  move instead of re-asking.

**What goes forward is the answer you do not have.** A genuine question is one
the code admits more than one defensible design for and no source you can reach
settles; that judgement is the human's, and their exit is `/design` on the PR
the row named, with the work order the answer implies already drafted. So is an
already-answered question whose pointer is a RECOLLECTION you could not verify
here: an unverified precedent is not an answer you can articulate, and a work
order resting on one is executed as though it were checked.

## Typed reads and the lens, and no shell at all

Every input about the PR arrives from a typed tool call — the queue row, the
context, the tree. Do not reach for `gh`, do not assemble a field by hand, and
do not answer any part of it from memory: a design decision reassembled by hand
is a decision whose inputs nobody can audit. If a tool is unavailable, say so
and stop — the answer is to connect the plugin's MCP server, not to work around
it.

The grant is five typed calls plus `Skill` and `Read`, and `Read` applies to the
`pr_checkout` tree and nothing else. Four are reads; `clone_release` disposes of
what this command itself created and writes no GitHub state.

The fifth, `human_rule`, is the send-back and the only call that writes GitHub
state. Typed for the same reason every input here is: its guards — the mandatory
work order, the head-sha anchor, clearing every other `ai:*` — live in the
binary rather than in whoever remembered them, and a ruling assembled by hand is
a ruling whose inputs nobody can audit. Reaching for `pr-review-report` through
a shell to make one is the defect the no-shell rule above names, not an
exception to it.

That list is a **declaration, not a sandbox**: measured on Claude Code 2.1.220,
a command granting only `Read` still ran a `Bash` call with no permission
denial. So the prohibition above is the thing that actually binds, which is why
it is written here rather than assumed of the frontmatter — and why nothing in
this file is fenced as a shell line, because what a reader copies out of a
command is what the command showed them.

## What `next_design` has already settled

**Which** question is next is not a second question. The queue is oldest-first
off the one enumeration, so the head of it already IS the next decision. Each
field of the row is one read a human otherwise does by hand:

- **`question.note`** — the trusted comment that raised the live question, whole
  (clipped; `noteTruncated` says when to read it via `pr_context`). The CLAIM,
  never a fact, and the sequence above is what checks it.
- **`question.source`** — who raised it: `vetter-verdict` (the vetter's
  `record-verdict design` note) or `producer-flag` (the producer's `flag-design`
  note). Provenance is by author, so a spoofed marker from a third party never
  reaches this row.
- **`question.at`** — when it was raised: the queue key, stated so the FIFO
  position is visible rather than taken on trust.
- **`question.sha` / `question.atHead`** — the head a vetter-raised question
  pinned itself to, and whether that is still the head. `atHead` false means the
  producer pushed past the question: read the current tree before treating the
  question as current. A producer-raised question pins no sha and the pair is
  null — a comparison nothing performed is not reported as a bool.
- **`headRefOid` and `baseRefName`** — the PR's own, never guessed.
- **`counts.noQuestion`** — where a labelled PR with no trusted raising comment
  went. There is no claim to check on such a row, so it is listed rather than
  presented, and presenting one anyway would hand the human an empty centre.

`queue.more` and `counts` frame the row. `counts.draft` is the PRs withheld from
the head because the code the question is about is still moving, named in
`withheld` rather than dropped; `counts.archivedRepo` is questions frozen in
archived repos, where no ruling can be written at all.

## Present the result, do not summarise it away

**Open with the row's `url`, verbatim, on its own line, before any analysis.**
Every form of this report carries it, the one-line form included: a finding the
reader cannot click through to is one they reconstruct a link for by hand.

**A genuine question gets the full presentation.** The rest of the row's fields,
then the independent read — what the issue asked, what the diff does, whether
the question is real against that — then the option space as the code constrains
it and the recommendation. Where the lens ran, its findings arrive under the
scope literal you declared, or you state that there was no lens. Then the
drafted work order the recommended ruling would carry.

**The other two are a few lines**: the finding named, the pointer where the
question is already answered or the correct state where it is misrouted, the
ruling you took, and the work order it carried. Say what you read to reach it —
never re-derive a settled question as if it were open.

Names collide across plugins; `/human-fsm:ndd` disambiguates.
