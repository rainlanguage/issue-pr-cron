# WORK-CLONES.md — the lifecycle, and the guards that hold it

The reference for the clone tools: what creates and destroys a work clone, the
path guards that make any other deletion inexpressible, the release decision the
attended release and the unattended sweep share, and the one result budget every
tool answers to. [CLAUDE.md](CLAUDE.md) holds the half that auto-loads — the FSM
framing, the tool surface, the invariants.

WHO READS THIS. Whoever changes a guard or a clone tool, and the two actors
whose clones these are: the producer, whose cwd is `$WORK_DIR` so it never
receives `CLAUDE.md`, and the vetter's dispatched `pr-auditor`, which is handed
the `dir` `pr_checkout` returned and reads nothing else. The one rule here that
decides a VERDICT is stated where the vetter actually meets it, in
`review-prompt.txt`: NEVER SEARCH FOR A CHECKOUT — the `dir` in `pr_checkout`'s
own result is the only path that is this PR's source.

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

One rule the unattended sweep does **not** share with release: an **audit-lens
checkout** (`vet-<repo>-<n>`, made by `pr_checkout`) is disposable on **age
alone** — one day, ignoring its PR state. The vetter checks out the PR it is
JUDGING, so that PR is always OPEN, and "open PR → active work" made every
leaked checkout immortal: 83 of them, 349 MB, under a sweep that had been
running nightly the whole time (#81). The dirt/unpushed guards still run first.
The sweep is also the ONLY thing that reclaims one — a run that dies is exactly
the run that leaks, so an end-of-run `clone_release` cannot be the mechanism —
which means the midnight `gc` line must name **every** clone root (`WORK_DIR`
_and_ the install dir), not just the first.

**A refusal must be a move the caller can make.** The over-budget error names
the argument that shrinks the result, and that argument is **declared on the
tool's own table entry** next to the schema advertising it, so the two cannot
disagree; a tool that declares none is told "NO argument makes this call
smaller" instead. It used to be a match over the call whose catch-all answered
`Some("limit")` for everything that was not `pr_context` — two of the seventeen
variants reaching it actually had one. `clone_list`, whose input schema was
`{}`, was told to lower a `limit` it did not accept, so the producer improvised
`ls -d …/*/ | wc -l` and reported a **count** where a state load belonged (#117)
— the same shape as #78, arriving through the guard written to stop #78. **The
advice a caller cannot follow is worse than a stated truncation**, because the
substitute it provokes is unstated; so both branches now forbid improvising, and
the "each refusal names a real argument" test walks the advertised tool table
instead of naming tools by hand.

**So an unbounded read is fixed in the read.** A tool whose result grows with
the box fits itself to the budget rather than waiting to be refused.
`clone_list` and `clone_gc` state the whole population as **counts that never
truncate** and offer their per-clone rows to the budget in the order a caller
acts on them — unreadable state, then unpushed commits, then dirty trees, then
releasable; for the sweep, errors and deletions before anything it kept — with
`listed`/`omitted` saying how many rows were taken. The sample shrinks; the
accounting does not.

**One result budget, and it must be under the harness's ceiling.** Every tool
result is checked against the same 36,000 bytes — `pr_context` included, which
used to get `max_diff_bytes + 32,000` (up to 332,000, about six times what the
harness accepts, so its guard never fired). Ordering is the mechanism: if the
harness speaks first the caller gets an untyped message with `is_error`
**unset**, and "a tool error is an instruction" stops applying exactly when it
is needed. The ceiling is measured against the running harness, never derived by
halving a payload that was refused; 2.1.220 has TWO untyped gates (a byte gate
around 50,011–50,176 bytes, not governed by `MAX_MCP_OUTPUT_TOKENS`, and a token
gate governed by it) and the budget sits ~28% under both. One budget for every
tool is also what makes narrowing CONVERGE — while the allowance scaled with
`max_diff_bytes`, lowering the argument lowered both sides equally. `pr_context`
fits itself to the budget rather than waiting to be refused, and reports
`diffBytes` / `diffIncluded` / `diffTruncated` so the shortfall is visible.

`pr_checkout` itself holds a binary postcondition: **the PR head at `dir`, or no
`dir`**. It fetches `refs/pull/<n>/head` into `refs/remotes/origin/pr/<n>`
(works on a shallow clone, works for forks, keeps the head provably pushed),
returns the `dir` and the `head` sha, and deletes what it made if any step
fails. Nothing downstream may search the filesystem for a checkout: the leftover
it finds is a different PR's code.
