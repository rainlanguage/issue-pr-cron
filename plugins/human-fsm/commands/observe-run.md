---
description: Force a producer or vetter run and watch it, measure what its context cost, read the retained trace corpus, and name the hand-roll worth tooling next — with the per-run counts that pick it, so a human can disagree.
argument-hint: producer|vetter <install-dir>
allowed-tools: Bash(pr-review-report:*)
---

Arguments: `$ARGUMENTS`

**ROLE** is the first argument — `producer` or `vetter` — and decides which
runner is forced, which log is watched, and which `runs/` directory is read.
**INSTALL-DIR** is the second, `/home/gildlab/issue-pr-cron` on this box.
Neither is guessed: a run forced against the wrong install dir spends real money
somewhere nobody is watching, and the tool refuses rather than defaulting.

Four things happen here, and the fourth is the point: force the run, watch it,
review what the trace corpus says runs are still hand-rolling, and **name the
optimization worth building next**. The first three are measurements. The fourth
is a recommendation, and it comes with the counts it rests on so the human
reading it can disagree from the same evidence.

## Every step is a typed call

The 2026-08-09 session did all of this by hand and got three of the mechanics
wrong — two of them twice in one afternoon. Every one now has a subcommand, and
re-deriving one in this turn with `tail`, `jq`, `grep` or a Python heredoc is
the same defect with an extra step.

## 1. Force the run, and watch it

```
pr-review-report force-run <role> --install-dir <install-dir>
```

One call does the two things that were hand-typed. It **fast-forwards the
install dir first** — the runner is a flake package built from that dir's own
git HEAD, so a run started against a stale checkout silently exercises code you
are not trying to observe, which happened twice on 2026-08-09 at a full run's
cost each time — and it **refuses, exit 3, if the checkout will not
fast-forward**. That refusal is a correctness stop and not a thing to force
past: report it and do not start a run.

Then it invokes the runner with `--force` and streams the run through the watch
filter, so what reaches you is the distiller's own lines (`·` narration, `▸`
tool calls, `⟹` results, `!` warnings) plus the runner's lifecycle lines, and
nothing else. Its exit code is the **run's**.

Start it in a **background** `Bash` call, and read that call's output file as
the run goes. A run outlives a foreground call: one is moved to the background
at the 600-second ceiling and hands back nothing of the stream when it is, while
the runs measured on 2026-08-10 took 32m 48s (producer) and 22m 30s (vetter). So
a foreground call buys ten minutes of blank screen and then backgrounds anyway,
which is how vetter run `20260810T102325Z` — fast-forwarded, both stops walked,
three auditors dispatched and working — came to be read as a hang and killed.
Backgrounded from the start it returns in seconds naming its output file, and
the fast-forward verdict, the `log:` line and `running:` are readable there
immediately. Read that file with the **Read** tool when you want to know where
the run is: it accumulates the stream line by line as the run writes it. One
read, then get on with something else — the run's exit arrives as a task
notification.

**Nothing may sit between the stream and you.** `force-run` flushes every line
as it prints it, and a pipe throws that away: `tail` and `head` both hold the
whole stream until the writer exits, which for a half-hour run is half an hour
of nothing. That was the first thing tried on 2026-08-10 and it showed nothing,
well before any timeout was reached. So pipe it into nothing at all — not
`tail`, not `head`, not a filter of any kind — and read the file.

If that output file is gone — a restarted session, or a run you did not start —
the `log:` line names where the same bytes are still being appended, and step
2's `watch-run` reattaches there.

`--force` walks **policy** stops and never **correctness** ones. It overrides
the `DISABLED` kill switch and a usage-gate PAUSE — both are the pipeline
choosing not to spend right now, and the human at the terminal owns that choice
for one run. It does not override the flock, or a usage-gate config REFUSAL. If
the run stops on one of those, that is the answer: report it and stop. Every
stop it does walk past lands on the run's `metrics/runs.jsonl` row as `forced`
plus the stop's own line, so this observation stays distinguishable from a paced
tick in the series it contributes to.

**Killing mid-run is sometimes right** — the 2026-08-09 run was killed once.
Stopping the background task kills the runner with it and releases the flock:
the whole process tree that task started is killed, `force-run` holds the runner
as its own child, and the lock is an open descriptor on that runner process, so
it goes when the process does. Probed on 2026-08-10 against a stand-in of the
same shape — a parent streaming a child that holds an `flock` on an open
descriptor — and stopping the task left neither process alive with the lock free
on the next attempt, both for a call backgrounded from the start and for one the
600-second ceiling backgrounded.

## 2. Reattach, if the stream was interrupted

```
pr-review-report watch-run <install-dir>/campaign.log
```

Only needed for a run this command did not start, or one whose stream was cut.
The vetter's log is `<install-dir>/review.log`. The follow starts at the log's
current end, so a line this run did not write cannot be attributed to it — the
bug that reported the previous run's `SKIP`/`END` as this run's, twice — and
there is no flag that turns that off. Exit **0** on the run's own
`END`/`SKIP`/`ABORT` line, **3** when `--timeout-secs` (default one hour)
stopped the watch first. A 3 means "still going", never "failed": a deadline
stops the watch, never the run.

## 3. Measure what the run cost

```
pr-review-report token-profile <install-dir>/runs/<run-id>.jsonl
```

From the trace's own `usage` events, never from narration. The trace path is on
the `run START` line the stream printed, so it is read from the run rather than
reconstructed. The reading is the SHAPE, not the total: a flat context with a
rising turn count is work happening off the main loop, and a context climbing in
step with turns is the inline pathology the prompt's benchmarks name. The
per-call figure is the one directly comparable to those benchmarks — 75,000
dispatching, 264,000 inline — and both print beside it.

## 4. Review the corpus, and read its recommendation

```
pr-review-report corpus-report <install-dir>/runs
```

Without this the recommendation would come from the most recent run, which is a
sample of one. On 2026-08-09 the newest run's most visible waste was a worker
downloading a tarball and writing a Python extraction script to read it —
compelling, expensive, and present in ONE trace. The answer it lost to appeared
in every run that took a screenshot item. Only the corpus tells those apart, and
the report does the telling:

- the per-run counts for every hand-rolled shape it measures, and how many
  traces it actually read, because `KEEP_RUNS` bounds the corpus and a
  rotated-out trace is not evidence of anything;
- each metric's `shape` over the full series;
- a **`BUILD NEXT:`** block naming the hand-roll worth tooling, the traces that
  exhibited it, the count in each, and everything it beat.

The rule it applies is worth understanding before you relay it. A hand-roll is
**shrinking** when the runs that _still do it_ do less of it than they used to —
the signature of a tool that already landed and is killing it — and shrinking
shapes are ruled out. Among what is left, the one seen in the **most traces**
wins, ties broken by the most recent sighting. Frequency is the discriminant
precisely because novelty is not: the tarball extraction is newer, bigger and at
its own peak, and it still loses to something last seen days earlier.

## 5. Present the evidence, then the recommendation

Print the run's outcome, its whole token profile, and the corpus table. Say what
each shows: which stops the force walked past, where the run's context sat
against the two benchmarks, and which hand-rolls are still being paid for and
how often.

Then **name the recommendation** — the `BUILD NEXT` metric, what it is, and why
it beat the others — and print the per-run counts underneath it. A
recommendation nobody can check is worse than a table, so the counts travel with
it: the human must be able to disagree from the same numbers, and where you
think the tool's pick is wrong, say so and say which count says so.

If the corpus recommends nothing, that is a real answer and not a failure — it
means every hand-rolled shape in the retained window is already shrinking, and
you should say that rather than promoting the least-bad row.

**This command writes nothing.** It opens no issue, applies no label, posts no
comment. The recommendation is a sentence for a human to act on, not a
transition.

## Typed calls, and no hand-rolled shell

Every measurement above is a subcommand **because** it was hand-written, and
each is independently useful outside this sequence — `token-profile` answers
"what did that run cost" for any trace, and `corpus-report` answers "what should
we build next" without forcing a run at all. If a subcommand is unavailable, say
so and stop: the answer is to install the binary this plugin's own manifest
names, not to reconstruct its output by hand and present the reconstruction as a
measurement.

Names collide across plugins; `/human-fsm:observe-run` disambiguates.
