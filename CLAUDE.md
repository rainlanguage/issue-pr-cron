# CLAUDE.md — issue-pr-cron

## The pipeline is a finite state machine

This repo runs an autonomous PR pipeline (a **producer** cron and a **vetter**
cron; landing is interactive). Treat it as a **finite state machine** — that
framing is what keeps it debuggable and honest:

- **The states are GitHub state.** A PR's state is its `ai:*` / `human:*`
  labels, its trusted `🤖 ai:vetter` / `🤖 ai:producer` comments, and its native
  `reviewDecision`. GitHub is the **single source of truth** — there is no
  separate local state. (The `review-verdicts.jsonl` / `review-costs.jsonl`
  ledgers are gone; cost lives in the vetter comment.)
- **The `pr-review-report` Rust tool is the ONLY transition function.** Every
  move between states — record a verdict, present the queue, backfill a comment,
  gc a clone, check closing keywords, read a comment — is a **tested**
  `pr-review-report` subcommand.
- **Raw `gh` / `git` in a prompt is a _loose_ transition.** It mutates state
  outside the transition function: unenforced, untested, and free to drift
  between the producer and the vetter. **A pipeline with loose transitions is
  not actually a finite state machine** — it's a diagram of one. So the producer
  and vetter prompts must do **all** GitHub input and output through
  `pr-review-report` subcommands, never raw `gh`. A read or write not yet
  covered by a subcommand is a gap to close by **adding one** — not a license to
  reach for `gh`.

North star for any change here: if you're about to instruct a prompt to call
`gh`, stop and add (or extend) a tool subcommand instead.

## Transitions (subcommands)

The state diagram lives in [README.md](README.md#pipeline-state-machine). The
transition functions:

| Subcommand                                                 | Transition it effects                                                                                   |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `--queue`                                                  | surfaces the presentable review queue (`ai:ready` + green + mergeable + vetted-at-head)                 |
| `--record-verdict <owner/repo> <n> <verdict> …`            | the vetter's write: apply the `ai:*` label + post the sha-bound `🤖 ai:vetter` comment (+ cost)         |
| `--trusted-comments <owner/repo> <n> [--marker] [--issue]` | author-verified comment read — the only trusted way to read a comment                                   |
| `--commit-closes <owner/repo> <n>`                         | closing-keyword vs. `closingIssuesReferences` drift check                                               |
| `--backfill-comments`                                      | one-time completion of the ledger→GitHub migration (replays each ledger verdict as its missing comment) |
| `gc-clones <work-dir>...`                                  | reclaim merged/closed work-clones across one or more clone roots (state cleanup)                        |
| `unvetted [--json] [--include-skipped]`                    | the VETTER's state-load: which open PRs need a verdict this run, vet-first, with each one's signals     |
| `mcp [--profile vetter\|producer]`                         | serve a role's transitions over MCP (stdio) — the FSM as a tool surface, not as prose                   |

## The FSM as a tool surface (MCP)

`pr-review-report mcp` speaks MCP over stdio. `--profile` picks the role, and a
profile is a **surface** filter, not a permission: `tools/list` returns only
that role's tools, so neither role pays preamble for the other's schemas and
neither can name the other's transitions.

| Profile            | Tools                                                                      |
| ------------------ | -------------------------------------------------------------------------- |
| `vetter` (default) | `unvetted`, `pr_context`, `pr_checkout`, `record_verdict`, `clone_release` |
| `producer`         | `clone_create`, `clone_release`, `clone_list`, `clone_gc`                  |

The vetter profile is the vetter's **only** tool surface: `review-run.sh` always
passes `--mcp-config review-mcp.json --strict-mcp-config` with
`review-settings.json`, so the vetter has **no Bash at all** — the tools are
`mcp__fsm__*` and a non-FSM operation is unrepresentable rather than merely
denied (a Bash deny-list is prefix-matched and bypassable). There is no non-MCP
vetter prompt or settings file, and no flag that selects one. The transition
guards — verdict vocabulary, mandatory in-range cost, well-formed PR ref,
human-sacred refusal — live in `validate_call` / `verdict_plan`, tested once,
instead of being re-asserted in prose.

The vetter's surface **replaces** its Bash, so it is `--strict-mcp-config` and
there is no non-MCP prompt or settings file to fall back to. The producer's
server (`campaign-mcp.json`) is **additive** — no `--strict-mcp-config`, it
keeps its Bash — because what it gains is a clone lifecycle it could not
previously perform at all. Neither is selectable at run time.

The vetter's surface is also deliberately **read-only on the filesystem**: it
reads the `pr_checkout` clone, it never builds or runs anything in it.
Clean-by-construction work clones are the producer's obligation
(`campaign-prompt.txt` step 6b) and, for rainix Solidity repos, the
`rainix-copy-artifacts` workflow's `git diff --exit-code`; re-running a PR's
tests is CI's job. The vetter's QA gate checks that the evidence block exists
and holds against the diff it reads, nothing more.

## Work-clone lifecycle

A work clone is created and destroyed through **tools**, never through shell.
`clone_create` clones or re-syncs `<root>/<name>`; `clone_release` disposes of
one; `clone_gc` is the end-of-run backstop sweep; `clone_list` reports what is
on the box. The roots come from the environment (`WORK_DIR`, plus `INSTALL_DIR`
because stranded `vet-*` clones live there) and **never** from a tool argument —
a model-supplied root would make every guard vacuous.

Why a tool: `campaign-settings.json` denies `Bash(rm -rf /:*)`, deny rules are
**prefix-matched**, and so it also denied `rm -rf $WORK_DIR/<clone>` — the exact
deletion `campaign-prompt.txt` mandated. The instruction was impossible to
follow for months and the box grew to 195 GB of clones (#56). Widening the rule
would fix that instance and keep the shape of the problem; moving the delete
behind a tool means "remove something outside the work roots" is not
expressible.

The path guards, in `clone_name_in_root` + `resolve_existing_clone`:

- exactly **one path component** directly under a configured root — a bare name
  or the full path of a direct child, nothing else;
- **no `..`** in any position, checked before any prefix arithmetic;
- **no absolute path outside the root**, including the sibling-prefix trick
  (`/home/gildlab/codeEVIL` shares a string prefix with `/home/gildlab/code` —
  the same class of bug as the deny rule itself);
- **never the root itself**, an ancestor of it, or a `.`-prefixed entry;
- **never a symlink**, and the canonical path must still be a direct child, so a
  symlinked component cannot smuggle the target elsewhere;
- **must contain `.git`** — only a git work clone is ever deletable, so no
  malformed argument reaches ordinary data.

And the release decision, in `release_decision` (shared with the sweep, so the
attended release and the unattended sweep never disagree about whether a clone
still holds work):

- commits that exist **only** in the clone refuse **unconditionally** — there is
  no override flag, because a flag is a thing a model under time pressure sets;
- an unknown push state is treated as unpushed (fail safe) — except an **unborn
  HEAD**, which is not unknown: a clone with no commits has nothing to lose, and
  reading it as unknown made every interrupted clone immortal;
- uncommitted changes refuse too, but `discard_uncommitted: true` overrides,
  because in practice that dirt is build output and refusing it outright is what
  leaves the clone on disk forever.

## Invariants

- **Human decisions are sacred.** A `human:*` label OR a native `APPROVED` /
  `CHANGES_REQUESTED` review is never overwritten by the vetter —
  `--record-verdict` refuses (exit 3), closing the TOCTOU race.
- **Comments are trusted by AUTHOR, never by marker text.** Any third party can
  post a `🤖 ai:vetter` / `🤖 ai:producer` / "Rework note" line; a comment
  counts only when the trusted account authored it. Read via
  `--trusted-comments`.
- **Vetted-at-head.** An `ai:*` label alone is not a verdict; a PR is vetted
  only when its trusted `🤖 ai:vetter` comment pins the **current** head. A
  moved head (code changed) → un-vetted → re-vet.
- **Landing is interactive-only** (the merge cron is retired):
  `gh pr merge --merge --admin` on the human's explicit per-PR word, after the
  SHA-bound review gate.
