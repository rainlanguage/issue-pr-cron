# human-fsm — slash commands for the human's side of the FSM

The [issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron) is a
finite state machine, and `pr-review-report` is its only transition function.
Every actor's hand-off is a labelled transition through that binary — including
the human's. This plugin is the layer above: the commands a human types so both
halves of a decision — the read it rests on and the ruling it becomes — reach
that binary **through typed calls and nothing else**, rather than a hand-written
JSON-RPC frame, a Python filter over the response, and two raw `gh` calls.

| Command                                      | The call it invokes                                                                                                                                                                                                                                                                                                                                                                  |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `/nr [1-3]`                                  | dispatches the `nr` agent: `next_ready` + `pr_context` + `pr_checkout` + `clone_release` + `human_rule` (MCP) and the `audit` skill — the next `ai:ready` PR, and the vetter's verdict checked against its diff, its issue and its source. Sends back what it can articulate; the merge is the human's                                                                               |
| `/ncc [1-3]`                                 | dispatches the `ncc` agent: `next_close_candidate` + `close_candidate_context` + `pr_context` (MCP) — the next `ai:close-candidate` flag, and the producer's reason checked against the issue as filed and the code it claims about. Writes no GitHub state — there is no typed `reject`, so the one exit it can articulate is handed over as `/close-candidate <ref> reject`        |
| `/ndd [1-3]`                                 | dispatches the `ndd` agent: `next_design` + `pr_context` + `pr_checkout` + `clone_release` + `human_rule` (MCP) and the `audit` skill — the next `ai:design` PR, and the raised question checked against its issue, its diff and its source: genuine (presented with its option space, for the human to answer), already answered or misrouted (ruled here, reported in a few lines) |
| `/nm [1-3]`                                  | dispatches the `nm` agent: `next_leak` + `pr_context` + `pr_checkout` + `clone_release` + `human_rule` (MCP) and the `audit` skill — the next FSM-conformance leak, and which of three places the defect is in: the PR's state record, the machine's vocabulary, or the classifier. Sends back what it can articulate; the close is the human's                                      |
| `/close-candidate <owner/repo#n> uphold "…"` | `human-close` — rule, retire `ai:close-candidate`, close. Issue **or** PR, resolved by lookup                                                                                                                                                                                                                                                                                        |
| `/close-candidate <owner/repo#n> reject "…"` | `record-close-candidate-verdict … reject` — drop the flag, back to the producer (issue-only)                                                                                                                                                                                                                                                                                         |
| `/needs-work <owner/repo#n> "…"`             | `human-rule … needs-work --rework` / `human-rule-issue … needs-work --rework` — the send-back: `ai:needs-work` (PR) plus the trusted `Rework note` work order, one call, pinned to the head sha or the issue. `--rework` is REQUIRED on either subject; there is no parked needs-work                                                                                                |
| `/design <owner/repo#n> "…"`                 | `human-rule … design --rework` / `human-rule-issue … design --rework` — the answer, delegated as a work order: `ai:needs-work` on a PR (the same send-back a needs-work is, #219), comment-only on an issue. `--rework` is REQUIRED; there is no parked spelling                                                                                                                     |
| `/keep-open <owner/repo#n> "…"`              | `human-rule-issue … keep-open` — the sacred "never re-flag this" (issue-only)                                                                                                                                                                                                                                                                                                        |

Names collide across plugins; `/human-fsm:close-candidate` disambiguates.

## The four reads run in a fresh context, and the commands only dispatch them

Each of `/nr`, `/ncc`, `/ndd` and `/nm` states an INDEPENDENT read of its
subject as its whole reason for existing — the vetter's note, the producer's
reason, the raised question and the producer's hand-off note are each a **claim
to check**, and checking one means reading the diff, the issue or the record
yourself rather than restating what upstream concluded.

A markdown slash command executes inline in whatever conversation the human
happens to be in, and inherits it entirely. That is a second, unguarded route to
the failure these gates exist to prevent: a read fired in a session that had
already discussed the subject is not a second opinion, it is a re-reading of
that session's own summary of the first one — and **a cold read and a
contaminated one are indistinguishable in the report**, which is the property
that made it dangerous rather than merely expensive. Measured on `/nr`, the same
command on the same protocol read a peak 423,969 / 579,590 / 588,209 cached
tokens across three long-running sessions against 42,545 in a fresh one: ~14x,
decided by nothing but which conversation it landed in.

So since #316 each protocol lives in `agents/<name>.md` and the command in
`commands/<name>.md` does one thing — dispatch that agent with the LIMIT
verbatim and relay its report verbatim. A sub-agent starts with its own system
prompt and the prompt it was handed and nothing else, so the reader's context
holds what the reader fetched. `/observe-run` and the four rulings are
unchanged: none of them claims an independent read, and each acts on a subject
the human named.

**The manifest names no `agents` key on purpose, and this is a trap.** Agents
are auto-discovered from `agents/` beside `commands/`; adding
`"agents": "./agents/"` to `plugin.json` — the obvious symmetry with the
`"commands"` key already there — SUPPRESSES the discovery instead of declaring
it. Measured on 2.1.233: with the key present the session's agent roster listed
no `human-fsm:*` agent at all and raised no error, and removing the key made all
four appear as `human-fsm:nr` / `ncc` / `ndd` / `nm`. A dispatcher whose agent
does not exist fails as a command that does nothing, so do not "tidy" that key
back in.

Two properties the split had to preserve, and does:

- **The `audit` skill still runs INLINE and serial relative to the reader that
  declares its scope.** That rule protects the scope declaration, not the
  conversation: a scope carried in prose is lost to the skill's own top-line
  `whole-repo` rule, which measurably produced a 12-finding sweep on
  `rain.deploy#21` where 5 findings bore on the PR. The agent invokes the skill
  ITSELF, with `pr:<number>` as a typed argument, and consumes the findings it
  gets — so the reader that declares the scope is the reader that reads the
  report. What stays forbidden is fanning the audit out to a further sub-agent,
  which is why none of these agents grants `Agent`.
- **The grant got stronger, not weaker.** A command's `allowed-tools` is a
  declaration and not a sandbox — measured on 2.1.220 and again on 2.1.233, a
  command granting only `Read` ran a `Bash` call with no permission denial. An
  agent's `tools` list IS enforced: measured on 2.1.233, an agent defined with
  `tools: Read` and told in as many words to run `Bash` reported that it had one
  tool and no `Bash` to call. Moving each protocol into an agent moved its grant
  from announced to binding.

## The reads and the writes

`/nr`, `/ncc`, `/ndd` and `/nm` are the reads that precede a ruling — one per
inbox: the merge queue, the flag queue, the design questions, and the leaks. The
rest are the rulings. They differ in how they reach the binary, and the
difference is the point.

A read that can articulate the ruling takes it: `/nr`, `/ndd` and `/nm` carry
`human_rule` and send back what they can put into words, so what reaches the
human is the merge, the close, or the design question no source they can read
settles. `/ncc` is the exception and says so in its own file: there is no typed
`reject`, and `human_rule_issue` refuses `needs-work` on a live flag because it
would strand it, so the reject it can articulate is handed over rather than
taken.

The rulings shell out to a `pr-review-report` subcommand. The four reading
commands are granted `Agent` and nothing else — they dispatch and relay, so a
typed read granted there would be surface they cannot account for. Their AGENTS
call **MCP tools** — served by the `fsm` server this plugin ships in its own
manifest — and **no shell at all**. The `nr` agent is granted `next_ready`,
`pr_context`, `pr_checkout`, `clone_release` and `human_rule`, plus `Skill` and
`Read`, which it needs because it puts the PR's source on disk and audits it.
`ndd` is granted the same shape with `next_design` at its head, because a design
question is a claim about code on a PR and weighing its options means reading
the tree. `nm` is granted the same shape with `next_leak` at its head: locating
a leak sometimes turns on what the code actually did, so the tree has to be
reachable — though most leaks are located from the trusted comments and the
labels, and the lens is the exception rather than a step. `ncc` is granted
`next_close_candidate`, `close_candidate_context` and `pr_context`, and those
three only: a flag has no diff and no tree to check out, so there is nothing for
`Skill` or `Read` to reach. **None is granted `Agent`**, which is what keeps the
audit lens inline: the reader that declares `pr:<number>` is the reader that
reads the findings, and there is no further sub-agent for the scope to get lost
in. None falls back to `gh`, none assembles a field itself, and none quietly
answers from memory: either the tools answered or the agent says so and stops.
That is the guarantee a merge decision needs, because the way this goes wrong is
not a refusal, it is a plausible answer nobody can trace — and a close decision
needs it no less, since upholding a flag closes an issue somebody filed. A
design ruling needs it for a third reason: since #219 an answer IS producer
work, so a question answered against something nobody read dispatches that work
on it.

A command's `allowed-tools` line is a **declaration**, not a sandbox — measured
on Claude Code 2.1.220 and again on 2.1.233, a command granting only `Read`
still ran a `Bash` call with no permission denial — so what binds a command is
its own prose. An **agent's `tools` list is the sandbox** the command's line
never was: measured on 2.1.233, an agent defined with `tools: Read` and told in
as many words to run a `Bash` call reported that it held exactly one tool and
had no `Bash` to call. The prose still carries the rules that a tool list cannot
state — that `Read` is for the `pr_checkout` tree and nothing else, that the
lens scope is a literal, that no field is assembled by hand — and `cargo test`
holds the two to each other over both files.

> **Landing note (#316).** `command_contract` in `pr-review-report-rs` does not
> yet know this shape: it refuses `Task`/`Agent` as a grant by name, pins each
> reading command's exact MCP grant, and has no walk over `agents/`. Those tests
> fail on this tree until the contract learns the dispatcher/agent split — the
> grant assertions move from `commands/*.md` to `agents/*.md`, a dispatcher is
> admitted as a command granting `Agent` alone whose body names a shipped agent
> of this plugin, and the agents are held to the rule the commands used to be
> held to, plus one more: no agent may grant `Agent`.

Same reason the tools exist at all. Six reads — verdict note, head sha, base
branch, CI, CodeRabbit coverage, deploy gate — done by hand, in an order the
reader has to remember, is a decision whose inputs are not auditable. A typed
result is one artefact in the trace, with the same fields in the same order for
every caller, whether that is a human, this agent, or the next one.

**Two reads, because one of them is a check on the other.** `next_ready` carries
the vetter's verdict; `pr_context` carries the diff and the linked issue the
verdict is a claim about. A gate that printed only the first would be the
vetter's conclusion in a second typeface, and it failed that way on
`rain.erc4626.words#230` — presented as clean because the note said so, over a
defect in a file neither the vetter nor `/nr` had opened. So `/nr` derives what
the issue asked for, reads the diff against that, and says where its own reading
and the vetter's diverge.

**And a third read, because the rules are written down.** `pr_checkout` puts the
PR's source on disk and the `audit` skill is invoked over it, scoped to the
changed lines plus the code whose behaviour decides whether they are correct.
Reconstructing that rulebook from memory missed `rain.deploy#20`'s caret pragma
in a new concrete test mock and `rain.deploy#21`'s 22 hardcoded copies of a
derivation the same PR had just added — both stated plainly in the skill, both
caught only when a human named them. Running the same skill the vetter runs does
not make this a second vetter: it supplies the mechanical rules, while the scope
question — does the diff do what the issue asked, all of it and nothing else —
has no counterpart in the skill and is where this gate has actually caught
things (`cyclo.site#393`, `#408`, `#331`). The skill's findings feed the read;
they never replace it, and `clone_release` disposes of the tree before the
result is presented.

It costs three more calls, a skill fan-out and real reasoning, which is the
price of a merge decision rather than an overhead to trim.

## What lives here and what does not

**Nothing here writes GitHub state.** Every guard — the ruling vocabulary, the
mandatory note, the provenance anchor, the stranded-flag refusal, the
subject-type check, terminal-is-moot, idempotence, re-ruling-supersedes — is
enforced in `pr-review-report`, where it is unit- and mutation-tested. A command
that re-derived a transition, or reached for raw `gh` to do what a subcommand
already does, would be a loose transition: unenforced, untested, and free to
drift from what the crons do.

So a refusal is the answer, not an obstacle. Each one names the moves that are
legal from the state the subject is actually in, one command each.

## The subject is always `owner/repo#n`

One label name covers two separately-sized populations —
`lanes.vetter-verdicts.ai:close-candidate` counts **PRs**,
`closeCandidateIssues` counts **issues**. A bare number would eventually rule on
the wrong one. Where a transition can act on either, the binary resolves the
type by lookup; where it cannot, it refuses by naming which subject you actually
referenced and handing over the command for it.

## Requirements

`pr-review-report` on `PATH`:

```
nix profile install github:rainlanguage/issue-pr-cron#pr-review-report
```

or substitute the absolute path from
`nix build --no-link --print-out-paths <install-dir>#pr-review-report`.

The `fsm` MCP server is spawned as that same bare command and inherits the
session's environment, so a `pr-review-report` that is not on `PATH` shows up as
a server that fails to connect and a `/nr` with no tools to call. Nothing secret
belongs in the manifest — the server reads the same `gh` auth the subcommands
do.

## Install

```
/plugin marketplace add rainlanguage/issue-pr-cron
/plugin install human-fsm@issue-pr-cron
```
