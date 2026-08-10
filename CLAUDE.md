# CLAUDE.md — issue-pr-cron

This file is a **router**, and the routing is a cost decision. Every byte of it
is re-read on every turn of any session whose working directory is this repo —
which is the VETTER's (`review-run.sh` cds to the install dir) and not the
producer's (`campaign-run.sh` cds to `$WORK_DIR`, which holds no `CLAUDE.md`).
Measured 2026-08-10 across two forced runs: first context 48,060 for the vetter
against 14,199 for the producer, with 41% of this file a CLI reference the
vetter has no `Bash` to invoke (#261).

So what stays here is what governs JUDGEMENT and binds EVERY reader — the FSM
framing, the tool surface, the invariants. Reference material a role reaches for
is a file of its own, and **every one of them is named right here**, because an
agent that never learns a rule does not error on it: it silently violates it,
and a file it does not know exists is one it never consults.

- **[TRANSITIONS.md](TRANSITIONS.md)** — the transition function as a CLI: every
  `pr-review-report` subcommand and the transition it effects, plus the
  slash-command layer a human types above them. Read it before invoking one, or
  before adding one.
- **[WORK-CLONES.md](WORK-CLONES.md)** — the clone lifecycle (`clone_create` /
  `clone_release` / `clone_gc` / `clone_list`), the path guards and the shared
  release decision that make a delete outside a work root inexpressible, and the
  one result budget every tool answers to. Read it before changing a guard or a
  clone tool.

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

**Where a subcommand cannot reach, a PreToolUse hook can — and the hook should
still BE a subcommand.** A tool surface binds only a session that was launched
with it, so it holds nothing for a session opened outside the cron, while the
loose `gh pr create` transition is reachable from every session on the box. That
is a reason to change WHERE the transition function is invoked from, not a
licence to write the guard in bash: `require-qa-block` is a `pr-review-report`
subcommand that Claude Code runs as a PreToolUse `Bash` hook (#83), so the gate
is tested, shipped in the flake closure, and covered by the nix build like every
other transition. `hooks/` holds what has not been converted yet — the two
deny-list bypass guards (`block-nix-wrap-gh.sh`, `block-cron-git-bypass.sh`),
tracked by #10. A hook is not an excuse to leave a transition loose; it is what
holds the invariant while it still is. See
[README.md](README.md#pretooluse-guards--what-a-prompt-cannot-hold).

**A gate on one edge needs a transition on the other.** `require-qa-block`
guards PR-**open**, so it cannot reach a PR already open — and when the vetter
started rejecting for a missing QA block, the producer had no move that fixed
one. Body edits were denied and no subcommand wrote one, so 122 of 160 open PRs
sat with a body the gate itself would refuse (#51). The answer is not to widen
the deny-list back out to the whole of `gh pr edit`; it is `repair-qa-block`, a
transition narrow enough to say what it does — **append** the section, never
rewrite the body — and validated with the **gate's own predicate**
(`carries_qa_block`), so what one writes is what the other accepts. **When a
guard names a defect, check that some transition can clear it**; a send-back
with no exit is a deadlock however correct the send-back is.

`ai:relink` was the same shape and went unnoticed for longer, because it was a
whole VERDICT rather than a rejection ground: it told the producer to change a
body `Closes #N` to `Refs #N`, and the producer's every body write was denied
(`Bash(gh pr edit:*)`) or absent (its MCP profile was four clone tools). One PR
sat in it. #135 retires the verdict — a linkage error is a `needs-work` whose
note names the reference, because it always named the same owner and the same
move — and #136 is the transition it never had, `weaken-closes`. The two had to
land together: consolidating alone would have moved an unexecutable instruction
into a bigger bucket, where it is harder to notice, which is the failure mode
this whole section is about.

**A tool that can only WEAKEN.** `weaken-closes` may rewrite `Closes` to `Refs`
and never the reverse, and that direction is the invariant rather than a
default: `Closes` is what GitHub resolves into `closingIssuesReferences`, and
`uncovered-issues` computes the producer's own backlog from that set. A producer
able to ADD one could mark an issue covered without fixing it — marking its own
homework, on its own inbox. Weakening can only ever GROW that inbox. It is held
three ways: the only text an edit carries is the `Refs` constant, the spans come
from the same scanner `commit-closes` uses, and the planner re-runs
`closing_keywords` over the result and refuses any plan whose closing set gained
a number.

**Both body repairs are producer TOOLS** (#136). `repair-qa-block` was reachable
only as a subcommand under the `Bash(pr-review-report:*)` allow rule, so the
producer's ability to write a PR body was a prefix-matched permission rather
than an enumerable transition — and two body repairs with two call shapes is
exactly the split a future reader has to guess at. Both are on the profile now,
and both keep their subcommand: a session opened outside the cron has no MCP
surface, and those are the sessions the QA retrofit exists for (#83).

**A PreToolUse guard is a guard against honest omission, not a security
boundary.** It reads a command line with a lexer that resolves quoting and
nothing else — it is not bash and never will be — so a determined bypass always
exists (a script file it cannot read, a variable it cannot expand). What these
guards buy is that the common ACCIDENT becomes impossible: forgetting the QA
block, or reaching for a `nix`-wrapped `gh` out of habit. Where the gate cannot
tell what a command does it refuses and says so, which is the right posture for
an accident but is not the same thing as enforcement. Do not cite one as proof
that a rule cannot be broken — cite it as proof that breaking it has to be
deliberate.

## The FSM as a tool surface (MCP)

`pr-review-report mcp` speaks MCP over stdio. `--profile` picks the role, and a
profile is a **surface** filter, not a permission: `tools/list` returns only
that role's tools, so neither role pays preamble for the other's schemas and
neither can name the other's transitions.

| Profile            | Tools                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `vetter` (default) | PRs: `unvetted`, `pr_context`, `pr_checkout`, `record_verdict`, `clone_release`. Close-candidate flags: `unvetted_close_candidates`, `close_candidate_context`, `record_close_candidate_verdict`                                                                                                                                                                                                                                                                          |
| `producer`         | `clone_create`, `clone_release`, `clone_list`, `clone_gc`, `push`, `open_pr`, `repair_qa_block`, `weaken_closes` — the clone lifecycle, the two OUTPUT edges (a moved head, a new PR), plus the two body repairs                                                                                                                                                                                                                                                          |
| `human`            | `next_ready`, `pr_context`, `pr_checkout`, `clone_release`, `next_close_candidate`, `close_candidate_context`, `next_design`, `next_leak`, `human_rule`, `human_rule_issue`, `human_close` — find the subject, read it, audit its source, rule on it. Each inbox has its own "which is next" read: `next_ready` for PRs, `next_close_candidate` for flags, `next_design` for design questions, `next_leak` for the FSM-conformance leaks — the inbox that should be EMPTY |

The vetter has **two subjects**, not one. A PR is judged on its diff; a producer
`ai:close-candidate` flag is judged on its evidence — and the second matters
because a wrong flag asks a human to destroy work. Both follow the same three
moves (state-load → read one → record one verdict) and the same
vetted-at-the-thing-judged rule: a PR is un-vetted again when its head moves, a
flag when the producer posts a new one. The vetter never writes a `human:*`
label in either subject; `uphold` leaves the flag for the human, `reject` strips
`ai:close-candidate` so the issue returns to the producer's queue.

The vetter profile is the vetter's **only** tool surface: `review-run.sh` always
passes `--mcp-config review-mcp.json --strict-mcp-config` with
`review-settings.json`, so the vetter has **no Bash at all** — the tools are
`mcp__fsm__*` and a non-FSM operation is unrepresentable rather than merely
denied (a Bash deny-list is prefix-matched and bypassable). There is no non-MCP
vetter prompt or settings file, and no flag that selects one. The transition
guards — verdict vocabulary, mandatory in-range cost, well-formed PR ref,
human-sacred refusal — live in `validate_call` / `verdict_plan`, tested once,
instead of being re-asserted in prose.

The vetter also **dispatches**, and that adds no transition (#257). Its audit
lens is deep source reading, and a main loop re-reads its whole history on every
turn, so the reading happens in a `pr-auditor` sub-agent whose context dies with
it — briefed from `review-auditor-prompt.txt` through `--agents`, exactly as the
producer briefs `pr-worker`. What comes back is EVIDENCE: the auditor's `tools`
list is the read half of this surface and names no write, the session deny-list
reaches inside it, and `record_verdict` stays a main-loop move. See
[Fanning the audit out](README.md#fanning-the-audit-out--and-keeping-the-verdict).

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

## Invariants

- **Human decisions protect AUTHORSHIP, and a ruling is an INPUT (#111).** No AI
  actor ever writes a `human:*` label, and none removes one as an OVERRIDE of
  the human: a native `APPROVED`/`CHANGES_REQUESTED` review, a `👤 human` ruling
  pinned to the CURRENT head is never overwritten by the vetter —
  `--record-verdict` refuses (exit 3), closing the TOCTOU race. But absolute
  parking is the ruled-out overreaction: a ruling is an input the machine
  EXECUTES. A ruling's trusted `Rework note` is the producer's work order; the
  push that executes it moves the head, the ruling stops describing the code by
  itself, and the PR re-enters vetting through the ordinary un-vetted path — no
  label of the human's own to clear (#219).
- **The send-back is ONE state, and the ruler rides on the comment
  (#133/#219).** `ai:needs-work` and `human:needs-work` demanded the same move
  from the same actor, so they are one state: `ai:needs-work`, whoever ruled —
  and an answered design question demands it again, so a human `design` ruling
  lands the same state (#219: the answer IS producer work; there is no parked
  spelling, and `human:design` is DELETED — a question still open is already
  `ai:design`). It is named for what it ASKS rather than for a verdict against
  the work (#230) — the producer reworks that same PR and branch, and the push
  is the transition. The label says what the work is; the **sha-pinned
  `👤 human` comment** says who said so — and which verb ruled — and that is
  where the authority lives. The vetter cannot forge one: `trusted_comments`
  authenticates by AUTHOR and matches the marker with `starts_with`, and every
  comment the vetter can post begins `🤖 ai:vetter` — a marker in the middle of
  a vetter note is body text. **The anchor is also the release.** A rework moves
  the head, the ruling stops describing the code, and the PR re-enters vetting
  through the ordinary un-vetted path with the ruling in
  `pr_context.humanComments`. `reworked-reject` is retired: its timestamp
  comparison proved only that SOME commit post-dated the label event, and what
  actually protects the objection is a stateless re-vet that can read it. So
  `human:*` means one thing — AUTHORSHIP-protected: never written by an AI
  actor, never removed as an override. On a PR neither string survives as a
  state: `human:design` was deleted outright (#219), and the RETIRED
  `human:needs-work` came out with `migrate-needs-work` once that one-shot had
  emptied it (#133/#230) — a migration is an execution vehicle, not permanent
  machinery. `human:needs-work` is still LIVE on an ISSUE, where
  `human-rule-issue needs-work` writes it.
- **A verdict accounts for every file the PR changes.** Scope coverage was the
  one thing `record_verdict` took on trust, and a verdict formed without a
  changed file in view is indistinguishable from a diligent one:
  `rain.erc4626.words#230` recorded `ready` while the PR was in the act of
  ADDING a file whose `address private immutable _ext;` breaks the audit skill's
  Solidity naming rule twice. So the claim is an ARGUMENT the tool checks, not a
  rule the prompt states. `covered` (MCP) / `--covered-file` (CLI) names each
  changed file; each **hand-written** one also carries an ANCHOR — a new-side
  diff line number plus its content — verified against the PR's own diff, which
  is the same move `Reviewed <sha>:` makes for the head one level up. Generated,
  vendored, lockfile and binary paths need the name alone, and so does any file
  the diff shows **no new-side line** for, because an anchor that cannot exist
  is a gate nothing can pass. The refusal (exit 4) reports **every** unmet entry
  at once and prints the line ranges that would satisfy it: the vetter cannot
  escalate to a human the way a producer can, so a correct verdict must never be
  more than ONE corrected call from being recorded. A PR that changes files
  whose diff carries not one `diff --git` header is refused outright: a claim
  checked against a diff that is not there is not checked, and a guard that
  silently stops firing is this very failure one level up, inside the thing
  built to prevent it.
- **The human's TERMINAL edge is a transition too — decide+do, no state between
  (#213).** `gh issue close` knows nothing about the FSM, so a hand-close left
  `ai:close-candidate` attached: 74 closed subjects org-wide (55 issues, 19 PRs)
  carried it when #94 was filed, a state no modeled transition produces.
  `human-close` is that edge as ONE transition — the pinned `👤 human` ruling
  comment, the close, then the flag retirement, in that order, writing NO label:
  the comment alone is the durable intent, and a tear between it and the close
  is the torn-close signature the vetter's state-load completes. It retires
  `ai:close-candidate` and no other `ai:*` label: that one means "a human still
  has to ACT", and closing is the act. Chaining a ruling tool with a Bash
  `gh close` would put the order and the flag clear in a prompt, which is
  exactly the half that was wrong every time.
- **A ruling names its POPULATION.** `ai:close-candidate` is ONE machine over
  both subject types (#211/#212), but on a PR the label has two distinct
  origins: a PRODUCER FLAG (a claim the vetter judges, whose reject returns the
  PR to the vet queue) or the VETTER'S OWN `close` verdict (already judged — the
  human's, via `human-close`; `record-close-candidate-verdict` refuses it
  because no producer claim exists there to judge). The inventories are the
  mixed `closeCandidateUnvetted` / `closeCandidateUpheld` arrays — no lane holds
  a flagged PR. Every human transition takes a full `owner/repo#n`; where it can
  act on either subject it READS the subject's `url` rather than trusting which
  command was typed, and where it cannot it refuses by naming what was
  referenced and printing the command that fits — in both directions.
- **A human ruling is a transition too, and it pins to what it ruled on.**
  `human-rule` / `human-rule-issue` are the only sanctioned way to write a
  `human:*` label: raw `gh issue edit --add-label` binds to nothing and, on an
  issue whose close-candidate flag has not been judged, permanently strands it
  (every AI transition refuses once a human has ruled). A ruling pins to the
  head sha, the live flag's timestamp, or the issue as filed — whichever anchor
  the AI's ruling on that same subject uses — so the two stale together.
- **Comments are trusted by AUTHOR, never by marker text.** Any third party can
  post a `🤖 ai:vetter` / `🤖 ai:producer` / "Rework note" line; a comment
  counts only when the trusted account authored it. Read via
  `--trusted-comments`.
- **Vetting is a pure function of the PR at its head, and `vetted_at_head` is
  its CACHE KEY.** A prior verdict is not an input to vetting, so there is no
  second kind of pass and no "awaiting re-vet" state — a PR is either vetted or
  **un-vetted**. A stored verdict may stand in for recomputing one only while
  both halves of the key hold: the trusted `🤖 ai:vetter` comment pins the
  **current head** (the input) **and** carries the **current `vet-protocol`
  stamp** (the function). A push invalidates the first, bumping `VET_PROTOCOL`
  invalidates the second for every verdict at once, and an unstamped comment is
  `VetProtocol::Unknown` — never current, because rules that cannot be
  identified are not the rules in force (`Merge::Unknown`,
  `CodeRabbitCoverage::Unreadable`). Bump the protocol when what vetting MEANS
  changes; nothing has to be pushed, relabelled or rewritten for the pipeline to
  recompute.
- **Landing is interactive-only** (the merge cron is retired):
  `gh pr merge --merge --admin` on the human's explicit per-PR word, after the
  SHA-bound review gate.
