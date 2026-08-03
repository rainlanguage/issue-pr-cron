---
description: The next ai:close-candidate flag to rule on — the producer's stated reason checked against the issue as filed and against the code it makes a claim about, rather than restated.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_close_candidate, mcp__plugin_human-fsm_fsm__close_candidate_context, mcp__plugin_human-fsm_fsm__pr_context
---

Arguments: `$ARGUMENTS`

**LIMIT** is the whole argument if present — how many flags to return. Omit it
and the tool defaults to 1.

This command is the human's **second opinion on a flag**, and the flag is a
proposal to **destroy work**: upholding one closes an issue somebody filed. A
gate that reprinted the producer's reason in a table would be that reason again,
and it would fail in exactly the case it exists for — the case where the reason
is wrong. `/nr` learned this on `rain.erc4626.words#230`, presented as clean at
length because the vetter's note said so; nothing about that failure is specific
to PRs.

So the producer's reason is a **claim to check**, and checking it means reading
the issue and the evidence yourself.

## What is different from `/nr`, and it is the core

`/nr` derives intent from an issue and checks a **diff** against it. A flag has
no diff. The claim is _"this issue should be closed"_ and the evidence for it is
one line the producer wrote, so the whole job is **falsifying that line**: read
what the issue actually asked for, read what the flag claims, then check the
claim against reality.

That check is not the same work for every claim, and the kind of claim decides
what would falsify it. Name the kind before you check it.

## The sequence

**1. `next_close_candidate`** — which flag is next, and what the producer and
the vetter each said about it. Call it with `limit` set only if the caller gave
one, and pass what they gave verbatim. The range is the binary's to enforce, as
every other guard in this plugin is; relay its refusal rather than rounding a
number down. Each ruling **retires its flag** — uphold-and-close, keep-open and
reject-the-flag all remove the row — so a page is stale past its head by
construction.

**2. `close_candidate_context`** on the flag that came back, addressed by the
row's own `issue` field, `owner/repo#n` — never a bare number, never a slug you
reassembled, and never the number off a title or a URL. One call per row if the
caller asked for several. It carries the issue BODY (step 1 does not), the whole
flag body, and every trusted `ai:producer` and `ai:vetter` comment unclipped —
step 1's `flag.reason` and `verdict.note` are clipped, and `reasonTruncated` /
`noteTruncated` say when.

**3. Derive the standard from the ISSUE, and do it before reading the flag.**
Say in your own words what the issue asked for and what would have to be TRUE of
the world for it to be closeable — working from the issue body alone. Not from
the flag reason, and not from the vetter's note: both were written by someone
who already had an answer, and a description of a claim is not a check on it.
Where the body asks for nothing legible — empty, a title only, a link and no
statement — that IS the finding. There is no standard to measure the claim
against, so no evidence can meet it, and saying so beats promoting the flag's
account of the issue into the standard the flag is judged by.

**4. Read the flag reason and NAME THE CLAIM'S KIND.** The producer writes
`Close-candidate: <category>: <evidence>`, and its vocabulary is exactly four
categories: `already-fixed-on-main`, `invalid`, `duplicate`, `wont-fix`. A
reason outside that shape is itself a finding — the flag was not written by the
transition that is supposed to write it. This is naming what is being CLAIMED so
you know what would refute it; it is not a new ruling vocabulary, and the
rulings at the end of this file are the only ones there are.

**5. Falsify it. The kind decides the instrument.**

- **already-fixed-on-main** — a claim about CODE, and the one that has actually
  been wrong. `pr_context` on the PR the reason names, then check the diff
  against the path and the behaviour the ISSUE named in step 3. `raindex#1348`
  is the recorded near-miss: a merged PR that looked like the fix touched
  `getAllDepositFields` and `getAllFieldDefinitions`, while the issue was about
  the `updateFields` path — a real PR, really merged, really adjacent, and not
  this issue's fix. Check three things and say which: the PR is **merged** (an
  open one is covered below, not fixed); its `closingIssuesReferences` or its
  changed files bear on **this** issue rather than a sibling; and the change
  does what the issue asked, not something in the same file. A PR that closed a
  DIFFERENT issue is evidence about that issue.
- **already-fixed-on-main about RENDERED behaviour** — the flag tool requires a
  screenshot URL, or a why-not naming the render ATTEMPT that failed, and a
  waiver of the form "pixel-identical" / "no visible effect" / "cosmetic only"
  is not one: it asserts the conclusion the render existed to establish. That
  exact shape has already been human-rejected (`cyclo.site#431`). A GUI claim
  carrying neither is unverifiable here, whatever the `file:line` beside it
  says.
- **already-fixed-on-main with nothing to read** — the reason names a bare
  commit, "main", "verified locally", or nothing at all. There is no typed read
  on this surface that reaches a commit or a branch tip, so the claim **cannot
  be checked here**. That is not a small gap to work around: it is the reject
  condition below, and it is reached honestly rather than by finding something
  adjacent to read.
- **duplicate** — the other issue must be READ, not assumed.
  `close_candidate_context` takes any `owner/repo#n`, so use it on the sibling:
  does it ask the same thing, and is it OPEN? A duplicate of a closed issue
  closes nothing, and two issues each named as the other's duplicate is a cycle
  that closes both.
- **invalid / wont-fix** — these make no claim about code, so nothing in a
  typed read refutes them. They are DESIGN positions, and agreeing with one is a
  decision the human makes on its merits rather than a check you can pass on
  their behalf. Say plainly that this is what you are doing; if the position is
  arguable, `/design` is the ruling that says so, not a quiet uphold.

**6. `openPr.coverage`, whatever the reason says.** `covered-by-open-pr` means
an open PR already claims to close this issue, and an open PR is **not a landed
fix** — the issue is not closeable while it is in flight, even if the flag's
reason is otherwise sound. This is the producer's own rule ("an issue merely
COVERED BY AN OPEN PR is NOT a close-candidate"), so a flag on such an issue is
already a flag that should not have been written, and saying so is the finding.
`unreadable` means the query failed and is treated as covered: it blocks, and
you say the query failed rather than reading a failure as an absence.

**7. Read the vetter's word as a claim too, and check `verdict.atFlag`.** Every
row here was upheld by the vetter — a rejected flag has its label stripped and
never reaches this queue — so agreement is the default and worth nothing by
itself. `verdict.atFlag` false means the vetter judged a **superseded** claim:
the producer re-flagged after that verdict, and the note describes evidence
nobody is offering any more.

**8. Put your read beside theirs and say plainly where they diverge.**
Agreement reached independently is worth something; agreement by restatement is
worth nothing, and the reader cannot tell which one they were handed unless you
say. A flag that does not survive a second read is the most valuable thing this
command produces, and burying it under the fields it invalidates is how an issue
gets closed anyway.

## A reason you cannot verify is a REJECT, not a close

Close only the cheap-and-clear candidates. Everything else goes back to the
producer as `/close-candidate <ref> reject` with the reason on the record —
that is a modelled transition: it strips `ai:close-candidate`, returns the issue
to the producer's uncovered queue, and the producer may re-flag on better
evidence. What must never happen is a bare de-flag that leaves no state behind,
and what must never happen for the opposite reason is upholding a claim because
checking it was hard.

"I could not check it" is a complete and correct outcome, stated as such. It is
not a reason to reach for a substitute read, and it is not the same as "the
claim is false".

## Why there is no audit skill here

`/nr` runs the `audit` skill because it has a diff, and the mechanical half of
judging code is written down in that skill rather than in a reviewer's memory.
Three things make this gate different, and the third is decisive.

**There is often nothing to point it at.** A flag is a claim about an ISSUE. An
invalid, duplicate or won't-fix flag makes no claim about code at all, and even
an already-fixed one names a change that may be a merged PR, a commit, or
nothing. A lens invoked because the sibling command invokes one is ceremony —
the exact thing `/nr` argues against — and it would take a checkout that then
has to be released to pay for a report nobody reads.

**There is no scope literal that fits.** The skill's whole vocabulary is
`whole-repo`, `pr:<number>` and `paths:<globs>`. This gate rules on an issue, and
none of those three names one. `pr:<number>` on a PR the flag merely cites scopes
the lens at something that is **not the subject of the ruling**, and `whole-repo`
is the sweep `/nr` spends a section refusing — twelve findings where five bore on
the question, on `rain.deploy#21`. Inventing a fourth spelling is free text with
a colon in it, which is the defect `#154` removed.

**And its output type cannot answer the question.** The skill reports PROBLEMS
and never says "works correctly" — that is its own stated rule. The flag question
is "does the thing the issue asked for now exist", and a lens that structurally
cannot confirm anything cannot confirm a fix. Every finding it returned would be
about code quality that no ruling here governs.

So this command swaps the lens for the read that actually falsifies the claim it
gets: `pr_context` on the PR the reason names, which is precisely the instrument
that would have caught `raindex#1348`. "No lens here, and here is why" is the
conclusion, not an omission — and if a future claim kind arrives that a lens
genuinely answers, this section is the argument to overturn deliberately rather
than a gap to fill quietly.

## Typed reads, and no shell at all

Every input arrives from a typed tool call — the queue row, the flag's context,
the PR the reason cites. Do not reach for `gh`, do not assemble a field by hand,
and do not answer any part of it from memory: a decision to close somebody's
issue, reassembled by hand, is a decision whose inputs nobody can audit. If a
tool is unavailable, say so and stop — the answer is to connect the plugin's MCP
server, not to work around it.

The grant is three typed reads and nothing else. All three are reads; this
command writes no GitHub state. That list is a **declaration, not a sandbox** —
measured on Claude Code 2.1.220, a command granting only `Read` still ran a
`Bash` call with no permission denial — so the prohibition above is the thing
that actually binds, which is why it is written here rather than assumed of the
frontmatter, and why nothing in this file is fenced as a shell line: what a
reader copies out of a command is what the command showed them.

## What `next_close_candidate` has already settled

**Which** flag is next is not a second question, and the order is this tool's
own decision rather than the PR queue's borrowed. Cheapest-first exists on the
PR side because merges are the scarce resource; here the scarce resource is
judgement, there is no cost signal in a one-line reason, and a flagged issue is
excluded from the producer's backlog — so a waiting flag is an issue that is
neither being fixed nor closed. The queue is therefore **oldest flag first**,
which bounds that limbo and rules on the evidence that has decayed most before
it decays further.

Each field of the row is one read a human otherwise does by hand:

- **`flag.reason`** — the producer's stated evidence. The CLAIM, never a fact,
  and step 5 is what checks it. `flag.at` is the anchor every record on this
  issue pins to.
- **`verdict`** — the vetter's word on that same claim, with `verdict.flagAt`
  and `atFlag` saying whether it judged THIS flag or a superseded one.
- **`openPr`** — `coverage`, the PRs named as refs you can hand straight to
  `pr_context`, and a `meaning` that says which way round the hazard runs.
- **`createdAt`** — the recency baseline. Evidence dated before the issue was
  filed cannot be the fix for it.
- **`labels`** and **`state`** — what else has been said about this issue, and
  that the flag is live rather than moot.

`queue.more` and `counts` frame the row. A flag you expected and did not get is
usually in `counts.unvetted` — the vetter has not judged it, and under the
3-item run cap it may wait; a flag the vetter would REJECT never arrives here at
all. `strandedFlags` is the list nothing else surfaces: a label with no producer
comment behind it, or a reject whose label removal did not land. No AI
transition clears either, so they sit until a human is told they exist.

## Present the result, do not summarise it away

Print every field of the row. Then give the independent read — what the issue
asked for, what the flag claims, what you checked it against and what you found
— then say where that and the producer's reason and the vetter's note diverge.
Then say what it adds up to.

The rulings are the existing ones and this command invents none:

- `/close-candidate <owner/repo#n> uphold <note>` — the claim holds: rule,
  retire the flag, close.
- `/close-candidate <owner/repo#n> reject <note>` — not on this evidence: the
  flag goes back to the producer, which may re-flag on better evidence.
- `/keep-open <owner/repo#n> <note>` — the **sacred** answer: this issue must
  never be flagged again. Not the same as reject, and the difference is whether
  the producer is allowed to try again.
- `/design <owner/repo#n> <note>` — the flag raises a question a human has to
  settle rather than a claim anyone can check.

**This command does not rule and does not close.** It is the read that precedes
the human's word.

Names collide across plugins; `/human-fsm:ncc` disambiguates.
