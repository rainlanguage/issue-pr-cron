---
description: The next unmodelled PR — an FSM leak the lane classifier buckets into no modeled state — located as a defect in exactly one of three places, against an independent read of the PR, its trusted comments, and the classifier's own rule. Runs in a fresh context.
argument-hint: [1-3]
allowed-tools: Agent
---

Arguments: `$ARGUMENTS`

Dispatch the `human-fsm:nm` agent, with `$ARGUMENTS` verbatim as the whole of
its prompt, and relay the report it returns verbatim. That is the entirety of
this command.

## Why the read is not here

`/nm` diagnoses the machine's own record-keeping: every PR in this queue is here
BECAUSE something about its record went wrong, so the note that implies its state
is the least trustworthy note in the pipeline. While this file carried the
protocol it executed inline in whatever conversation you happened to be in and
inherited it entirely — and a diagnosis made partly from a conversation the
machine holds no copy of is exactly the unauditable input this queue exists to
find. **Nothing in the output distinguished the two.**

The human ruled the read into a fresh context (#316), the same ruling `/nr`
carries and for the same reason. A sub-agent starts with its own system prompt
and the prompt it was handed and nothing else, so the agent is the mechanism and
the protocol lives in `agents/nm.md`.

**The LIMIT is the whole argument** and it is the whole of what the dispatch
carries. Pass it verbatim and pass nothing else: no PR you have in mind, no
producer note you already read, no location you had already guessed. A guessed
re-filing is indistinguishable from a located one — that is this queue's own
standing hazard, and a contaminated dispatch is a new way to reach it.

## What this command must not do

**It does not read, and it does not rule.** Every typed call — the leak row, the
PR context, the checkout, its release, the send-back — belongs to the agent,
which is why none of them is granted here. Do not reach for `gh`, do not look
the PR up to "check" the report, and do not answer any part of it from memory: a
state reached by hand edits is the unauditable record this queue diagnoses.

**It relays; it does not summarise.** The agent's report is written for the
human — every field of the row, which of the three places the defect is in, the
send-back it took, and what it read to conclude it. An empty queue is the
HEALTHY answer and is relayed as such, `counts.leakUnknown` included. Add
nothing of your own.

**The close stays the human's**, on a PR they named, and nothing in this command
or that agent may take it.

If the agent cannot be dispatched, say so and stop. The answer is to install the
plugin so its agent is loadable, not to run the protocol here.

Names collide across plugins; `/human-fsm:nm` disambiguates.
