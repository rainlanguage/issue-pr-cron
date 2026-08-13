---
name: rain-repo-conventions
description: The standing constraints on any agent doing work in a rainlanguage repo, so a brief does not have to restate them — what is never done, what is true of the box and the harness rather than of the work, and what is a route around a defect that should be fixed. Covers clone and scratch isolation for parallel agents, the irreversible acts reserved to the human, the `## QA` gate every `gh pr create` passes through, the force-backgrounding of long builds and how to read one to completion, what makes a wait terminate, and the soldeer bump sequence. Invoke once per session, before the first clone or the first write. Triggers on "work this issue", "open a PR", "clone the repo to work in", "bump a dependency", "run the test suite", "wait for CI", "CI is red", "report the result".
version: 0.1.0
---

# Rain repo conventions

These bind every agent working in a rainlanguage repo, whether or not its brief
restates them, and they are why a brief does not have to. Nothing here is a
pipeline transition, a verdict, or a tool: it is what is true around the work.

**Where this and a repo's own `CLAUDE.md` disagree, the repo wins** — it is
closer to the code and it is versioned with it. Where this and the brief you
were given disagree, **say so and stop**; picking one silently is the failure
either way round.

Three groups, and the group tells you what kind of rule you are reading:

- **Rules** are chosen. Nothing about the box lifts one and none of them
  expires.
- **Facts** are true of the environment, not of the work. Each is stated so you
  can check it — when the environment changes the entry is wrong, and a wrong
  fact carried as belief is worse than no entry at all.
- **Workarounds** route around a defect somewhere else. Each names what would
  retire it. Fixing the defect and deleting the entry is always the better
  move.

## Rules

- **One fresh clone per agent invocation, at a path no other agent is using.**
  Never a git worktree: worktrees share one repository — its refs, its config
  and its object store — so two agents in two worktrees are one agent with two
  prompts. A distinct clone path is the only isolation the filesystem actually
  enforces, and the failure it prevents is silent cross-contamination rather
  than an error anyone sees.
- **Scratch files and logs go under a path scoped to the repo being worked,
  never a shared session scratchpad.** Parallel agents are handed the same
  scratchpad, so an unqualified filename is one another agent is also writing:
  what you read back is another repo's numbers, and you will report them as
  this repo's. A count, a log or a diff read out of a shared path is evidence
  for nothing.
- **Never merge a PR, delete a branch, create a tag, dispatch a deploy workflow
  or broadcast a transaction. Never force-push, in any spelling.** Every one of
  them either destroys work the next agent cannot recover or moves state a
  human then has to live with. Performing one is taking a decision, not doing
  the work you were asked for.
- **Assign every PR and issue you open to `thedavidmeister`.** An unassigned
  subject has no inbox: it is found only by someone already looking for it.
- **Never depart from an agreed spec without asking first — including to turn a
  red CI green.** A red that reflects reality is information, and the damage in
  papering over it is that the information is destroyed rather than read. Ask,
  and say what the red actually reports.
- **Never report a result you did not watch finish.** "Green" requires the
  pass/fail line in front of you — not an exit code you inferred, not a run you
  started and left, not a suite that was green on the last head. A claim about a
  run you did not read to its end is fabricated whatever the run goes on to do.
- **A wait is bounded, and never keyed on a pattern its own command line
  contains.** Every loop carries a maximum iteration count and says what it last
  saw when it reaches it, because an unbounded loop whose condition never
  arrives does not fail — it runs on past the turn that started it, invisible
  and unattended, and they accumulate. The self-matching pattern below is the
  other way one never terminates. Prefer neither shape: poll once in the
  foreground and move on, or read the backgrounded output file. A loop is the
  last resort, not the default.

## Facts

- **`gh pr create` passes through a PreToolUse gate that reads the body before
  anything is created.** The body must carry a literal `## QA` heading and these
  four literal lines:

  ```
  ## QA
  - Discriminating tests:
  - Mutations applied:
  - Oracle:
  - Category check:
  ```

  An honest `n/a` **with its reason** satisfies a line the change genuinely
  cannot have; an absent line does not. The gate checks the block is PRESENT —
  what the four lines have to say, and whether their claims hold, is the
  producer QA guide's business and the vetter's, not this file's.

- **Push the branch before `gh pr create`.** The create is where the gate reads
  the body, and a create against a branch the remote does not have either fails
  outright or drags an interactive push prompt into a session that cannot
  answer one.
- **That gate reads a command line, not a shell.** `--body-file` must be a
  literal absolute path: a `$VAR` or a `~` in the argument is never expanded,
  because the gate resolves quoting and nothing else. This one is not a defect
  awaiting a fix — a guard that expanded shell variables would have to BE a
  shell, with every bypass that implies.
- **A long `nix`, `forge` or `cargo` command may be force-backgrounded by a
  hook.** The call then returns immediately with an output file path in place of
  the command's output. Poll that file with `Read` until the command's own
  completion line is in it: the return is not the result, and neither is the
  last line of a file still being written.
- **`jq` is on PATH only inside the flake devshell.** Outside one it is simply
  absent, and a pipeline through it fails in a way that reads like the `gh` call
  failing. `gh --jq` takes the same expressions and needs nothing on PATH.
- **`pgrep -f <pattern>` matches the searching process's own command line.** The
  pattern sits in the argv of the shell doing the looking, so the search finds
  itself and goes on finding itself after the process it is watching is gone. A
  wait keyed on it can never be satisfied in the direction it needs.

## Workarounds

- **Bump a soldeer dependency with `soldeer update`, never `soldeer install`.**
  `install` resolves from the lock rather than from the bumped manifest and dies
  in the remappings step, with an error that names the remapping instead of the
  resolution that produced it. `update` then **appends** the new remapping
  without pruning the one it replaces: delete the stale line by hand, and check
  what still imports through it — a versioned import prefix is deliberate, so a
  leftover remapping keeps compiling and pins the old version silently.
  _Retired when a bump resolves from the manifest and rewrites the remapping in
  place._
- **Never commit a `.pre-commit-config.yaml`.** Entering a rainix devshell
  generates one in the repo root — it is output, regenerated on every entry, and
  it looks authored because it arrives untracked next to real files. Much of the
  org already `.gitignore`s it and the rest does not. _Retired per repo by that
  `.gitignore` line, which is the fix worth making the moment you meet a repo
  without it._
