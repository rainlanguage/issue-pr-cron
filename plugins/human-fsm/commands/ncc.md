---
description: The next ai:close-candidate flag to rule on — the producer's stated reason checked against the issue as filed and against the code it makes a claim about, rather than restated. Runs in a fresh context.
argument-hint: [1-3]
allowed-tools: Agent
---

Arguments: `$ARGUMENTS`

Dispatch the `human-fsm:ncc` agent and relay the report it returns verbatim.
That is the entirety of this command.

Its prompt is exactly one line and carries exactly one value:

- an argument was given — `LIMIT: $ARGUMENTS`, the argument verbatim,
  unparsed and unrounded, because the range is the binary's to enforce and not
  this command's to pre-judge;
- no argument was given — `LIMIT: none`, and the agent lets the tool default
  to 1.

Say it that way rather than passing the bare argument, and never dispatch an
EMPTY prompt: measured, an agent handed an empty turn reads it as having been
sent no task at all and reports that instead of running the protocol. `LIMIT:
none` is a value; an empty string is an absence, and the two are different
instructions.

## Why the read is not here

A flag is a proposal to **destroy work**: upholding one closes an issue somebody
filed. `/ncc` exists to falsify the producer's one-line reason, and falsifying
it means reading the issue as filed and the evidence the reason cites yourself.
While this file carried the protocol it executed inline in whatever conversation
you happened to be in and inherited it entirely, so a read fired in a session
that had already discussed the issue was that session's account of the flag,
checked against itself. **Nothing in the output distinguished the two.**

`/ncc` runs no audit lens and takes no checkout, and neither fact touches this:
the contamination route is the inherited conversation, not the lens. The premise
is what carries it, and the premise here is the same independent read `/nr`
claims — with a higher cost of being wrong, because a close is not a state a
later reader sends back. So the human's ruling (#316) applies here too. A
sub-agent starts with its own system prompt and the prompt it was handed and
nothing else, so the agent is the mechanism and the protocol lives in
`agents/ncc.md`.

**The LIMIT is the whole argument** and it is the whole of what the dispatch
carries. Pass it verbatim and pass nothing else: no issue you have in mind, no
reason you already read, no ruling you were leaning toward.

## What this command must not do

**It does not read, and it does not rule.** All three typed reads belong to the
agent, which is why none is granted here. Do not reach for `gh`, do not look the
issue up to "check" the report, and do not answer any part of it from memory: a
decision to close somebody's issue, reassembled by hand, is a decision whose
inputs nobody can audit.

**It relays; it does not summarise.** The agent's report is written for the
human — every field of the row, what the issue asked for, what the flag claims,
what was checked against it, and where that diverges from the producer's reason
and the vetter's note. "I could not check it" is a complete outcome and is
relayed as one, never smoothed into a verdict. Add nothing of your own.

**This command does not close.** Two exits stay the human's because both destroy
or freeze work — `uphold`, which closes somebody's issue, and `keep-open`, which
forbids the producer from ever flagging it again — and the reject the agent can
articulate is handed over as `/close-candidate <ref> reject <note>`, since no
typed reject exists yet. Relay that hand-over as the agent stated it.

If the agent cannot be dispatched, say so and stop. The answer is to install the
plugin so its agent is loadable, not to run the protocol here.

Names collide across plugins; `/human-fsm:ncc` disambiguates.
