---
description: The next unmodelled PR — an FSM leak the lane classifier buckets into no modeled state — located as a defect in exactly one of three places, against an independent read of the PR, its trusted comments, and the classifier's own rule.
argument-hint: [1-3]
allowed-tools: mcp__plugin_human-fsm_fsm__next_leak, mcp__plugin_human-fsm_fsm__pr_context, mcp__plugin_human-fsm_fsm__pr_checkout, mcp__plugin_human-fsm_fsm__clone_release, Skill, Read
---

Arguments: `$ARGUMENTS`

**LIMIT** is the whole argument if present — how many leaks to return. Omit it
and the tool defaults to 1.

This command is the human's read on the queue that should be **empty**. `/nr`
rules on work the machine finished, `/ncc` on work the machine wants destroyed;
this one reads the PRs the machine cannot SAY anything about — a trusted
producer note implies a state was handed off, and no label records one, so the
lane classifier buckets the PR nowhere. The dashboard renders that set as the
`leaks` box with the act "model it", and its correct size is ZERO.

So a leak is not a queue item to process. It is a **defect to locate**, and "not
modelled" always means something is wrong in exactly one of three places. The
deliverable of this command is naming which:

- **The PR's state record is wrong.** It belongs in an existing state and the
  label went missing or was hand-mangled. The finding is the state it belongs
  in, and the ONE command that files it there, written out in full so the human
  can type it:
  - `/human-fsm:reject <owner/repo#n> <note>` — work is owed on it; the note is
    the work order, and the producer is the next mover.
  - `/human-fsm:design <owner/repo#n> <note>` — the note is the ANSWER to a
    question the PR raises, which is itself producer work.
  - `/human-fsm:close-candidate <owner/repo#n> uphold <note>` — the PR is
    finished or should be destroyed. This is the close: it runs `human-close`,
    which resolves PR-or-issue by lookup, posts the ruling, closes the subject,
    and retires any pending flag. It does NOT need an `ai:close-candidate` flag
    to already exist, which matters here because a leaked PR by definition
    carries no `ai:*` label at all.

  One command, because a state reached by a sequence of hand edits is a state
  nothing can audit, which is how this PR got here.

  The commonest cause is a label the classifier NO LONGER RECOGNISES. Note the
  distinction, because it is the difference between a leak and a false alarm: a
  state the FSM **retired** is usually still bucketed on purpose — the label
  stays in the classifier precisely so the PRs already wearing it stay visible
  in their old lane — and such a PR is in a modeled state and will never appear
  here. It is a state **deleted** from the classifier's vocabulary that leaks:
  once no arm matches the label, the PR falls through every lane, and it does so
  the moment the deletion lands rather than when anyone touches the PR. So a
  cluster of leaks all wearing one dead label is not a producer that misbehaved;
  it is a migration that removed a state and left its population behind.
- **The machine's vocabulary is missing a state.** The PR is in a real condition
  no label covers — the producer had something true to say and no legal way to
  say it. That is a design gap, and the modeled response is the design path: the
  finding names the missing state and its consuming transition, and the `design`
  ruling carries that answer straight to the producer as its work order. Never
  an ad-hoc label — a label the classifier does not know is this same leak with
  extra steps.
- **The classifier is wrong.** The PR IS in a modeled condition and the
  machinery fails to see it. The fix is the classifier, never the instance:
  hand-patching the PR would clear the box while leaving the defect armed for
  the next PR shaped like it. The finding names the defect precisely enough to
  file, and it is filed as an issue on the pipeline repo — by the human, since
  this command writes nothing.

Naming a state and its consuming transition is the whole job. A leak "processed"
with a plausible label and no diagnosis is the machine's account of itself
degrading one PR at a time — the exact thing the conformance metric exists to
catch.

## The sequence

**1. `next_leak`** — which PR leaked, and the evidence. Call it with `limit` set
only if the caller gave one, and pass what they gave verbatim. The range is the
binary's to enforce, as every other guard in this plugin is; relay its refusal
rather than rounding a number down. Each located leak changes this queue — the
PR is re-filed, or the vocabulary grows, or the classifier fix moves the whole
set — so a page is stale past its head.

**An empty result is the HEALTHY answer, and the tool says so.** `health` reads
`healthy-…` exactly when zero leaks were found over a fully-read population;
report that as the good outcome it is, in as many words, and stop. It is not an
error, not a shrug, and not a reason to go looking somewhere else for leaks the
machinery did not report — a second enumeration would be a second definition of
"leak". The one caveat is the tool's own: `counts.leakUnknown` above zero means
some PRs' comments could not be read and `health` says `…-not-proven-health` —
report the zero AND the unknowns, never the zero alone.

**2. `pr_context`** on the PR that came back, addressed by the row's own `pr`
field, `owner/repo#n` — never a bare number, never a slug you reassembled. One
call per row if the caller asked for several. It carries the PR body, the diff,
the linked issues, and every trusted `ai:producer` / `ai:vetter` comment
UNCLIPPED — the row's `has.producerNote` is clipped, `producerNoteTruncated`
says when, and the full thread here is where the truncated half lives. The
thread is the leak's history: what the producer thought it was handing off, and
what every actor before it recorded.

**3. Read the evidence the row carries, both halves, and say what each
implies.** `has.producerNote` is the trusted note that made this a leak — the
producer acting, usually naming the state it MEANT ("rework pushed", "blocked on
…", "ready for review"). `lacks.aiStateLabel` is the classifier's own read of
the labels, `null` by computation rather than assertion, with the full label
list beside it — a leftover non-state label, or a stale `human:*` one, is often
the first clue which of the three defects this is. The note is a CLAIM about
intent, not a fact about state: a producer note saying "ready" on a PR whose
diff does not build is two defects, not one.

**4. Check the classifier's actual rule against that evidence — before
concluding anything.** The rule is one sentence and it is this: an open producer
PR with NO `ai:*` label whose newest trusted hand-off marker is a
`🤖 ai:producer` note is a leak; a vetter blocked-on CLEARANCE as the newest
marker is the modeled transition into un-vetted instead, never a leak. Hold the
row's evidence against that rule as written. If the evidence does not actually
satisfy it — the note is not the newest marker, the label list contradicts
`aiStateLabel`, the "producer note" is something else wearing the prefix — the
leak is the CLASSIFIER's defect (the third place), and you have found it without
opening the diff. If the evidence does satisfy the rule, the defect is in the
first or second place, and the question becomes which state this PR is really
in.

**5. Decide between the three, from the PR rather than from the note.** The
producer's note names the state it intended; whether the PR IS in that condition
is a fact about the code and the thread, not about the sentence. Where the
determination turns on code — is this rework actually pushed, does the claimed
fix exist, is the "blocked" dependency real — **`pr_checkout`** the same
`owner/repo#n` and read the tree with `Read`; cross-check the `head` it returns
against `pr_context`'s `headRefOid` before reading a line, and never search for
a checkout — the `dir` in `pr_checkout`'s own result is the only path that is
this PR's source. The `audit` skill is available through `Skill` for the case
where locating the defect needs the org's review rules run over that source,
with the scope declared as `pr:<number>` exactly as `/nr` declares it — but it
is an instrument for that case, not a step: most leaks are located from the
thread and the labels, and a lens invoked as ceremony is the thing `/nr` spends
a section refusing.

**6. `clone_release`** the checkout, passing the `dir` name `pr_checkout`
returned, before you present anything — this server has no `WORK_DIR`, so its
clones land in the temp-dir fallback no sweep may reach, and unreleased
checkouts are how this box filled its disk. If `pr_checkout` ERRORED there is
nothing to release: re-call it ONCE, and if it fails again present the read
without that half and say so in as many words.

**7. Present the location, not a disposition.** Print every field of the row.
Then say which of the three places the defect is in and the finding that follows
from it — the state and its ONE filing command, or the missing state and its
consuming transition for the design path, or the classifier defect stated
precisely enough to file. Then say what you checked to conclude it: the note,
the labels, the rule, and the code where you read any. A leak whose location you
cannot determine is a complete and correct outcome STATED AS SUCH — say what you
read and what would decide it, rather than defaulting to the nearest plausible
label, because a guessed re-filing is indistinguishable from a located one and
wrong in the way nothing downstream can detect.

## Why this is a second opinion and not a dispatcher

The tempting shape for this command is a router: read the note, apply the label
the note names, next. That shape fails in exactly the case the queue exists for.
Every leak is here BECAUSE the machine's account of it broke — the one thing
known for certain about this PR is that something in the record-keeping around
it went wrong — so the note that implies its state is the least trustworthy note
in the whole pipeline, and reprinting it as the finding would launder the defect
into the ledger. The three-way split is what forces the diagnosis: a router only
ever produces the first answer, and the second and third are the ones that fix
anything permanently. The vocabulary gap and the classifier defect each produce
every future leak of their shape; the mangled record produces only itself.

The rulings this command precedes are the existing ones and it invents none —
these five are the whole shipped set, and a command named outside them cannot be
typed:

- `/human-fsm:reject <owner/repo#n> <note>` — files the PR into rework with the
  order on the record.
- `/human-fsm:design <owner/repo#n> <note>` — the note is the answer, and the
  answer travels to the producer as its work order in the same act.
- `/human-fsm:close-candidate <owner/repo#n> uphold <note>` — ends what is
  finished or should be destroyed: rule, close, retire any pending flag.
- `/human-fsm:close-candidate <owner/repo#n> reject <note>` — sends a flag back
  to the producer (a flagged subject only, so never a leak's exit).
- `/human-fsm:keep-open <owner/repo#n> <note>` — protects an ISSUE from being
  re-flagged; not a move on a PR.

**This command does not rule, does not label, and does not file** — it is the
read that precedes the human's word, and on this queue the human's word is a
diagnosis.

## Typed reads, and no shell at all

Every input arrives from a typed tool call — the queue row, the context, the
tree. Do not reach for `gh`, do not assemble a field by hand, and do not answer
any part of it from memory: a diagnosis of the machine's own record-keeping,
reassembled by hand outside the machine, is the same defect it is diagnosing. If
a tool is unavailable, say so and stop — the answer is to connect the plugin's
MCP server, not to work around it.

The grant is four typed calls plus `Skill` and `Read`, and `Read` applies to the
`pr_checkout` tree and nothing else. All four typed calls are reads except
`clone_release`, which disposes of what this command itself created and writes
no GitHub state. That list is a **declaration, not a sandbox**: measured on
Claude Code 2.1.220, a command granting only `Read` still ran a `Bash` call with
no permission denial. So the prohibition above is the thing that actually binds,
which is why it is written here rather than assumed of the frontmatter — and why
nothing in this file is fenced as a shell line, because what a reader copies out
of a command is what the command showed them.

## What `next_leak` has already settled

**Which** PR is not a second question, and neither is what "leak" means. The
population is the LANE CLASSIFIER's own verdict — the same call that decides
every other PR's state — so this command, the `human-queue` array and the
dashboard box cannot hold three opinions about which PRs escaped the machine.
That is not decoration: while the enumeration was a second reading of the labels
(`no ai:* label`) it swept in every PR parked in the human-decisions lane, which
are in a modeled state, waiting on exactly the human running this command.

The queue is **oldest first**. A leak is in nobody's queue — not the producer's,
not the vetter's, no human inbox but this one — so nothing else will ever
surface it, and the harm is proportional to how long it sits. Newest-first is
what the underlying search returns and what the tool deliberately does not use:
it buries the most-rotted leak below the page cap, which is exactly what it did
before this order was chosen.

Each field of the row is one read a human otherwise does by hand:

- **`createdAt`** — the age this queue is ordered by, on the row so a human
  reading a page can re-rank it by eye.
- **`classifier.lane` / `classifier.state`** — the lane classifier's verdict for
  this PR, recomputed live. `leak`/`leak` on an honest row. Anything else means
  the enumeration and the definition have come apart, and THAT is the finding —
  the PR is in the state named there, and this command's job is to say so rather
  than diagnose a leak that is not one.
- **`has.producerNote`** — the trusted note that made this PR a leak, with
  `producerNoteBytes` / `producerNoteTruncated` saying when the full text lives
  in `pr_context` instead. The note is the producer's claim about what it did;
  step 5 is what checks the claim.
- **`lacks.aiStateLabel`** — the state label the classifier looked for and did
  not find. `null` is the ordinary reading here.
- **`lacks.labels`** — everything the PR carries instead, whole, with
  `labelsTruncated` declared. What survived on the PR is evidence about what was
  mangled, and a dead label here is the migration case above.
- **`health`** — what this queue's size means, typed. `healthy-…` is the zero
  this box exists to read; `…-not-proven-health` is a zero over unread PRs,
  never silently promoted; `leaking-…` is the state this command works.
- **`counts`** — the population behind the verdict: `totalProducerPrs`,
  `leakCandidates` (the PRs the classifier could still file as leaks — what the
  comment read was paid for), `leaks`, `leakUnknown` (comment reads that failed,
  each named in `fetchErrors`), and `archivedRepoPrs` (frozen, not rulable, not
  this queue's work).
- **`queue.more`** — the leaks this page left behind, each as much a defect as
  the head, and each older than nothing else in the queue.

## Present the result, do not summarise it away

Print every field of the row. Then the independent read: what the note implies,
what the labels say, whether the classifier's rule as written actually fires on
this evidence, and what the code showed where you read it. Then the location —
one of three places, named — and the finding in that place's own terms. Where
the evidence and the note diverge, that is the headline and not a footnote: a
leak whose producer note misdescribes the PR is two defects, and burying the
second under the first is how it leaks again.

Names collide across plugins; `/human-fsm:nm` disambiguates.
