---
description: The next ai:design PR to rule on — the raised design question checked against the issue, the diff and the code it is about, then presented with its code-constrained option space where the answer is the human's, or ruled here where it is already answered or misrouted. Runs in a fresh context.
argument-hint: [1-3]
allowed-tools: Agent
---

Arguments: `$ARGUMENTS`

Dispatch the `human-fsm:ndd` agent and relay the report it returns verbatim.
That is the entirety of this command.

Its prompt is exactly one line and carries exactly one value:

- an argument was given — `LIMIT: $ARGUMENTS`, the argument verbatim, unparsed
  and unrounded, because the range is the binary's to enforce and not this
  command's to pre-judge;
- no argument was given — `LIMIT: none`, and the agent lets the tool default
  to 1.

Say it that way rather than passing the bare argument, and never dispatch an
EMPTY prompt: measured, an agent handed an empty turn reads it as having been
sent no task at all and reports that instead of running the protocol.
`LIMIT:
none` is a value; an empty string is an absence, and the two are
different instructions.

## Why the read is not here

`/ndd` exists to check a claim — the claim that a human decision is genuinely
required — and checking it means reading the issue, the diff and the code
yourself. While this file carried the protocol it executed inline in whatever
conversation you happened to be in and inherited it entirely, so a read fired in
a session that had already discussed the PR was that session's framing read back
as a check on itself. **Nothing in the output distinguished the two.** Framing
is the thing this gate tests: a question inherits its framing, and a reader who
inherited the same framing has no lever on it.

The human ruled the read into a fresh context (#316), the same ruling `/nr`
carries and for the same reason. A sub-agent starts with its own system prompt
and the prompt it was handed and nothing else, so the agent is the mechanism and
the protocol lives in `agents/ndd.md`.

**The LIMIT is the whole argument** and it is the whole of what the dispatch
carries. Pass it verbatim and pass nothing else: no PR you have in mind, no
question you already read, no answer you were leaning toward. A dispatch that
carries a view of the question re-contaminates the read this shape exists to
keep clean, and it does it invisibly.

## What this command must not do

**It does not read, and it does not rule.** Every typed call — the queue row,
the PR context, the checkout, its release, the send-back — belongs to the agent,
which is why none of them is granted here. Do not reach for `gh`, do not look
the PR up to "check" the report, and do not answer any part of it from memory: a
design ruling dispatches producer work (#219), so an answer reassembled from
this conversation's context dispatches work on it.

**It relays; it does not summarise.** The agent's report is written for the
human — the row, the independent read, the option space as the code constrains
it, and the ruling it took where the question was already answered or misrouted.
Add nothing of your own, and relay a tool that was unavailable or a lens that
never ran as the agent stated it.

**The answer to a genuine question is still the human's**, and nothing in this
command or that agent may take it.

If the agent cannot be dispatched, say so and stop. The answer is to install the
plugin so its agent is loadable, not to run the protocol here.

Names collide across plugins; `/human-fsm:ndd` disambiguates.
