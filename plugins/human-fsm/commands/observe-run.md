---
description: Force a producer or vetter run and watch it, measure what its context cost, and read the retained trace corpus for what a run is still hand-rolling — the evidence a human picks the next subcommand to build from.
argument-hint: producer|vetter [install-dir]
allowed-tools: Bash(pr-review-report:*), Bash(nix run:*), Bash(git:*)
---

Arguments: `$ARGUMENTS`

**ROLE** is the first argument — `producer` or `vetter` — and decides which
runner is forced, which log is watched, and which `runs/` directory is read.
**INSTALL-DIR** is the second if present, defaulting to
`/home/gildlab/issue-pr-cron`. Neither is guessed from the box: a run forced
against the wrong install dir spends real money somewhere nobody is watching.

This command **gathers and measures. It does not decide.** What to build next
from what it surfaces is a judgement about the machine's own shape, which is why
this is a `human-fsm` command and not a cron. Every step below is one call; none
of them takes a verdict, and the last section is explicit that producing one
here would be the failure mode, not the deliverable.

## What each step is, and what it got wrong by hand

The 2026-08-09 session performed all five of these by hand and got three of them
wrong — two of them twice in one afternoon. Each mechanic that has a subcommand
below is there **because** it was hand-written, and re-deriving one in this turn
is the same defect with an extra step.

## 1. Fast-forward the install dir, before anything else

The runner is a flake package built from the install dir's **own git HEAD**, so
a forced run against a stale checkout exercises code that is not what you are
trying to observe, silently and with a full run's spend. That happened twice on
2026-08-09. Fast-forward the install dir with
`git -C <install-dir> pull --ff-only` and say what it moved to; a checkout that
will not fast-forward is a stop, not a thing to force past — report it and do
not start a run.

## 2. Force the run

The runners take exactly one argument. For the producer it is
`nix run git+file://<install-dir>#campaign-run -- --force`, for the vetter
`nix run git+file://<install-dir>#review-run -- --force`. Both honour `DISABLED`
without it; the observation run is the deliberate exception, which is what
`--force` is for.

`--force` walks **policy** stops and never **correctness** ones. It overrides
the `DISABLED` kill switch and a usage-gate PAUSE — both are the pipeline
choosing not to spend right now, and the human at the terminal owns that choice
for one run. It does not override the flock (two runs of a role corrupt each
other's clones and GitHub state) or a usage-gate config REFUSAL (the tick would
run on config nobody validated). If the run stops on one of those, that is the
answer: report it and stop.

Start it in the **background**, because step 3 is the thing that reports on it,
and a foreground runner holds the turn for the whole run with nothing on stdout
that the watcher is not about to show you better.

Every stop this run walks past lands on its `metrics/runs.jsonl` row as `forced`
plus the stop's own line, so this observation is distinguishable from a paced
tick in the series it contributes to. Nothing here has to do that; it is already
true.

## 3. Watch it

Not a `grep` you write. The filter is non-obvious in exactly the way that costs
an afternoon — it must carry the distiller's own prefixes (`·` narration, `▸`
tool calls, `⟹` results, `!` warnings) as well as the runner's lifecycle lines,
and it must not replay the log's existing tail as events for the current run.
Both were got wrong by hand and each produced a false report: once "nothing more
will surface until it ends" while narration was streaming, and once the previous
run's `SKIP`/`END` arriving as this run's.

```
pr-review-report watch-run <install-dir>/campaign.log
```

The vetter's log is `<install-dir>/review.log`. The follow starts at the log's
current end, so a line this run did not write cannot be attributed to it, and
there is no flag that turns that off. It exits **0** on the run's own
`END`/`SKIP`/`ABORT` line and **3** when `--timeout-secs` (default one hour)
stopped the watch first — a deadline stops the WATCH, never the run, so a 3
means "still going", not "failed".

Run it in a **foreground** `Bash` call. It blocks until the run ends, which is
the whole point; wrapping it in anything that returns immediately abandons the
wait.

**Killing mid-run is sometimes right** — the 2026-08-09 run was killed once —
and what you kill is the RUNNER, not the watcher. The flock is an open
descriptor on the runner process, so it is released when that process dies;
killing the watcher leaves the run going and blind. The watcher will report the
run's `END` line if you kill the runner while it is still following.

## 4. Measure what the run cost

From the trace's own `usage` events, never from narration. This is the missing
sibling of `distill-trace`, `trace-outcome`, `run-metrics` and `run-timings`,
and it was hand-written in Python twice on 2026-08-09 — once mid-run and once at
the end — because there was no way to ask.

```
pr-review-report token-profile <install-dir>/runs/<run-id>.jsonl
```

The trace path is on the `run START` line the watcher printed, so it is read
from the run rather than reconstructed. The reading is the SHAPE, not the total:
a flat context with a rising turn count is work happening off the main loop, and
a context climbing in step with turns is the inline pathology the prompt's
benchmarks name. The per-call figure is the one directly comparable to those
benchmarks — 75,000 dispatching, 264,000 inline — and both are printed beside
it.

## 5. Mine the corpus

Without this step the recommendation comes from the most recent run, which is a
sample of one. On 2026-08-09 the newest run's most visible waste was a worker
downloading a tarball and writing a Python extraction script to discover a
package's contents — compelling, and present in **two of twenty-one traces**.
The thing that was actually worth building appeared in every run that took a
screenshot item. Building the first would have been the wrong call, and only the
corpus says so.

```
pr-review-report corpus-report <install-dir>/runs
```

Read the per-run series, not the totals. A count falling to zero and staying
there is a pathology some landed tool already retired; a count that holds is the
one still being paid for. The report types each metric's `shape` and names the
newest run that exhibited it — and it says how many traces it actually read,
because `KEEP_RUNS` bounds the corpus and a rotated-out trace is not evidence of
anything.

The `shape` column is a fact about the series and nothing more. A zero can mean
a tool retired the hand-roll or that this run had no occasion for it, and the
counts cannot tell those apart — on 2026-08-09 raw `gh api` and the
render-harness rebuild both ended at zero and only one of them was solved. That
distinction is the reading, and the reading is yours.

## 6. Present the evidence, and stop short of the verdict

Print the run's outcome, its whole token profile, and the corpus table with the
per-metric shapes. Then say what each one shows: which stops the force walked
past, where the run's context sat against the two benchmarks, which hand-rolls
are still being paid for and how often, and which of them the corpus cannot
distinguish from a quiet spell.

**Do not name the next subcommand to build.** That is the human's call, it is
the reason this command exists at all rather than a cron, and a recommendation
manufactured from a single reading is exactly the sample-of-one this whole
sequence is built to avoid. If a candidate looks obvious, say what the evidence
would have to show to make it obvious, and let the human say the word.

Anything the human then decides to file is filed by the human. This command
opens no issue, writes no label, and posts no comment.

## Typed calls, and no hand-rolled shell

The three measurements above are subcommands **because** they were hand-written,
and each is independently useful outside this sequence — the profile answers
"what did that run cost" for any trace, and the corpus report answers "what is
still being hand-rolled" without forcing a run at all. Do not re-derive one of
them in this turn with `tail`, `jq`, `grep` or a Python heredoc. If a subcommand
is unavailable, say so and stop: the answer is to install the binary this
plugin's own manifest names, not to reconstruct its output by hand and present
the reconstruction as a measurement.

Names collide across plugins; `/human-fsm:observe-run` disambiguates.
