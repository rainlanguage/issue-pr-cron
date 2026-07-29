# human-fsm — slash commands for the human's side of the FSM

The [issue→PR pipeline](https://github.com/rainlanguage/issue-pr-cron) is a
finite state machine, and `pr-review-report` is its only transition function.
Every actor's hand-off is a labelled transition through that binary — including
the human's. This plugin is the layer above: the commands a human types so both
halves of a decision — the read it rests on and the ruling it becomes — reach
that binary **through typed calls and nothing else**, rather than a hand-written
JSON-RPC frame, a Python filter over the response, and two raw `gh` calls.

| Command                                      | The call it invokes                                                                                                                      |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `/nr [1-3]`                                  | `next_ready` + `pr_context` (MCP) — the next `ai:ready` PR, and the vetter's verdict checked against its diff. **Reads**; writes nothing |
| `/close-candidate <owner/repo#n> uphold "…"` | `human-close` — rule, retire `ai:close-candidate`, close. Issue **or** PR, resolved by lookup                                            |
| `/close-candidate <owner/repo#n> reject "…"` | `record-close-candidate-verdict … reject` — drop the flag, back to the producer (issue-only)                                             |
| `/reject <owner/repo#n> "…"`                 | `human-rule` / `human-rule-issue` — `human:reject`, pinned to the head sha or the issue                                                  |
| `/design <owner/repo#n> "…"`                 | `human-rule` / `human-rule-issue` — `human:design`                                                                                       |
| `/keep-open <owner/repo#n> "…"`              | `human-rule-issue … keep-open` — the sacred "never re-flag this" (issue-only)                                                            |

Names collide across plugins; `/human-fsm:close-candidate` disambiguates.

## The read and the writes

`/nr` is the read that precedes a ruling; the rest are the rulings. They differ
in how they reach the binary, and the difference is the point.

The rulings shell out to a `pr-review-report` subcommand. `/nr` calls **MCP
tools** — `next_ready` and `pr_context`, served by the `fsm` server this plugin
ships in its own manifest — and its `allowed-tools` is those two and **nothing
else**. It cannot fall back to `gh`, cannot assemble a field itself, and cannot
quietly answer from memory: either the tools answered or the command fails
loudly. That is the guarantee a merge decision needs, because the way this goes
wrong is not a refusal, it is a plausible answer nobody can trace.

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
and the vetter's diverge. It costs a second call and real reasoning, which is
the price of a merge decision rather than an overhead to trim.

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
