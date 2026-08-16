---
description: The next ai:ready PR to rule on — the vetter's verdict, checked against an independent read of the diff, the issue it claims to close, and the audit skill run over the PR's own source at a declared pr:<number> scope. Runs in a fresh context.
argument-hint: [1-3]
allowed-tools: Agent
---

Arguments: `$ARGUMENTS`

Dispatch the `human-fsm:nr` agent, with `$ARGUMENTS` verbatim as the whole of
its prompt, and relay the report it returns verbatim. That is the entirety of
this command.

## Why the read is not here

`/nr`'s stated reason for existing is that the vetter's note is a **claim to
check**, and that checking it means reading the diff and the issue yourself.
While this file carried the protocol it executed inline in whatever conversation
you happened to be in, and inherited that conversation entirely — so a read
fired in a session that had already discussed the PR was a re-reading of that
session's own summary of the vetter, presented as a second opinion. **Nothing in
the output distinguished the two.** A cold read and a contaminated one are the
same report, which is what made it dangerous rather than merely expensive.

The human ruled that read into a fresh context (#316). A sub-agent starts with
its own system prompt and the prompt it was handed and nothing else — no
conversation history, no earlier turns, no view about this PR formed before it
began — so the agent is the mechanism, and the protocol lives in
`agents/nr.md` where the reader that executes it can be given a context of its
own. Measured, the cost of the old shape was ~14x: four runs of the same
command on the same protocol read a peak 423,969 / 579,590 / 588,209 cached
tokens in long-running sessions against 42,545 in a fresh one, determined by
nothing but which conversation the command landed in.

**The LIMIT is the whole argument** — how many PRs to return, the binary's range
to enforce — and it is the whole of what the dispatch carries. Pass it verbatim
and pass nothing else: no PR you have in mind, no verdict you already read, no
ruling you were leaning toward. A dispatch that carries a view of the PR
re-contaminates the read this shape exists to keep clean, one paragraph at a
time, and it does it invisibly.

## What this command must not do

**It does not read, and it does not rule.** Every typed call — the queue row,
the PR context, the checkout, its release, the send-back — belongs to the agent,
which is why none of them is granted here. Do not reach for `gh`, do not look
the PR up to "check" the report, and do not answer any part of it from memory:
a merge decision reassembled by hand is a decision whose inputs nobody can
audit, and a second reader working from this conversation's context is the exact
contamination the dispatch removed.

**It relays; it does not summarise.** The agent's report is written for the
human — every field of the row, the independent read, the lens's findings under
the scope literal they were formed at, and where all of that diverges from the
vetter's note. Condensing it here restores the failure the report is shaped to
prevent: the divergence is the headline, and a summary is where a headline goes
to become a footnote. Add nothing of your own, and if the agent reports that a
tool was unavailable or a lens never ran, relay that too rather than smoothing
it.

The agent rules what it can articulate — `ai:needs-work`, with the work order on
the record — and it says so in its report. **The merge is still the human's**,
on a PR they named, and nothing in this command or that agent may take it.

If the agent cannot be dispatched, say so and stop. The answer is to install the
plugin so its agent is loadable, not to run the protocol here.

Names collide across plugins; `/human-fsm:nr` disambiguates.
