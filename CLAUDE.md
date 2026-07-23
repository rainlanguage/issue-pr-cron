# CLAUDE.md — issue-pr-cron

## The pipeline is a finite state machine

This repo runs an autonomous PR pipeline (a **producer** cron and a **vetter**
cron; landing is interactive). Treat it as a **finite state machine** — that
framing is what keeps it debuggable and honest:

- **The states are GitHub state.** A PR's state is its `ai:*` / `human:*`
  labels, its trusted `🤖 ai:vetter` / `🤖 ai:producer` comments, and its native
  `reviewDecision`. GitHub is the **single source of truth** — there is no
  separate local state. (The old `review-verdicts.jsonl` / `review-costs.jsonl`
  ledgers are being removed; cost moves into the vetter comment.)
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
| `--gc-clones <work-dir>`                                   | reclaim merged/closed work-clones (state cleanup)                                                       |
| `unvetted [--json] [--include-skipped]`                    | the VETTER's state-load: which open PRs need a verdict this run, vet-first, with each one's signals     |
| `mcp`                                                      | serve the vetter's transitions over MCP (stdio) — the FSM as a tool surface, not as prose               |

## The FSM as a tool surface (MCP)

`pr-review-report mcp` speaks MCP over stdio and exposes the **vetter's** whole
job as four tools — `unvetted` (state-load), `pr_context` (read one PR),
`pr_checkout` (local source for the audit lens), `record_verdict` (the only
write). This is the vetter's **only** tool surface: `review-run.sh` always
passes `--mcp-config review-mcp.json --strict-mcp-config` with
`review-settings.json`, so the vetter has **no Bash at all** — the tools are
`mcp__fsm__*` and a non-FSM operation is unrepresentable rather than merely
denied (a Bash deny-list is prefix-matched and bypassable). There is no non-MCP
vetter prompt or settings file, and no flag that selects one. The transition
guards — verdict vocabulary, mandatory in-range cost, well-formed PR ref,
human-sacred refusal — live in `validate_call` / `verdict_plan`, tested once,
instead of being re-asserted in prose.

The surface is kept deliberately small — a wrapper per `gh` command would cost
more context than the prose it replaces. It is also deliberately **read-only on
the filesystem**: the vetter reads the `pr_checkout` clone, it never builds or
runs anything in it. Clean-by-construction work clones are the producer's
obligation (`campaign-prompt.txt` step 6b) and, for rainix Solidity repos, the
`rainix-copy-artifacts` workflow's `git diff --exit-code`; re-running a PR's
tests is CI's job. The vetter's QA gate checks that the evidence block exists
and holds against the diff it reads, nothing more.

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
