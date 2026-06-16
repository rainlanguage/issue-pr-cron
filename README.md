# rainlanguage org issue→PR cron

A local, autonomous routine that opens fix PRs for open issues across the
rainlanguage GitHub org. Runs on a persistent box via cron; recovers all state
from GitHub each run (no server-side memory).

## Scope — read this first

**The ONLY org-mutating action this routine takes is `gh pr create`** (plus
`gh pr comment` to attach a UI screenshot). It **never** merges, deploys, or
closes/edits/comments-on issues. If it believes an issue should be closed
(already fixed, invalid, duplicate) it records a *close-candidate* — it never
acts on it. A human reviews and disposes. This is enforced two ways: the
permission deny-list in `campaign-settings.json` and the rules in
`campaign-prompt.txt` (step 7 / 7a).

## Files (tracked here)

| File | Purpose |
|------|---------|
| `campaign-run.sh` | Durable runner: `flock` single-run lock, `DISABLED` kill-switch, `timeout`, bakes PATH+nix, invokes `claude --print` with the prompt + settings, logs to `campaign.log` (+ per-run JSONL traces in `runs/`). |
| `campaign-prompt.txt` | The campaign instructions fed to the model. |
| `campaign-settings.json` | Tool allow/deny list passed via `--settings` (the permission guardrails). |
| `cron.env.example` | Template for deployment-specific values (PR assignee, work dir, model, run caps). Copy to `cron.env` (gitignored) and edit. |

## Configuration

Deployment-specific values are **not** committed. Copy `cron.env.example` to
`cron.env` (gitignored) and set at least `PR_ASSIGNEE` (the GitHub handle every
opened PR is assigned to). `WORK_DIR`, `MODEL`, `MAXTIME`, `KEEP_RUNS` have
defaults and may be overridden there. The runner self-locates its install dir
and rebuilds `PATH`/nix from `$HOME`, so there are no machine paths in the repo;
`campaign-prompt.txt` uses `{{WORK_DIR}}` / `{{CLOSE_CANDIDATES}}` / `{{ASSIGNEE}}`
placeholders that the runner substitutes at run time.

## Runtime state (NOT tracked — see `.gitignore`)

- `campaign.log` — distilled human-readable log (`tail -f` to watch).
- `runs/<ts>.jsonl` — full per-run stream-json traces (`KEEP_RUNS` most recent).
- `close-candidates.jsonl` — append-only queue of issues the cron thinks should
  be closed but won't touch. A human reviews it like a PR queue and closes
  deliberately. One JSON line per candidate:
  `{repo, issue, url, title, reason, evidence, found_at}`.
- `DISABLED` — presence pauses the cron (kill-switch).
- `campaign.lock` — flock file (prevents overlapping runs).

## Schedule & controls

- **crontab:** `0 1,5,9,13,17,21 * * * <install-dir>/campaign-run.sh`
  (every 4h).
- **Pause:** `touch DISABLED`  ·  **Resume:** `rm DISABLED`
- **Watch:** `tail -f campaign.log`  ·  **Run now:** run `campaign-run.sh` directly.

## What a run does

1. Auth + toolchain check (`gh auth status`, nix `forge --version`); stop loudly if broken.
2. Enumerate open issues org-wide.
3. Cheaply dedup against open PRs (single `jq` pass; byte-grepping the PR JSON is forbidden).
4. For each tractable, genuinely-uncovered issue: clone, branch, implement a
   minimal fix with mutation-validated tests, build + test, open ONE PR per issue
   (`gh pr create --assignee $PR_ASSIGNEE`, body `Closes #N` / `Refs #N`).
   If already fixed on main → no PR, log a close-candidate.
5. UI PRs require a screenshot (headless chromium harness → `pr-screenshots` branch).
6. End with a summary: PRs opened, issues skipped, close-candidates logged.
