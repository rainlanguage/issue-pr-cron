// pr-review-report — report every open PR (and logged close-candidate) that needs a HUMAN decision,
// RESPECTING reviews already done: it overlays (a) recorded review verdicts in review-verdicts.jsonl
// and (b) GitHub's own review state (APPROVED / CHANGES_REQUESTED) on top of the CI/mergeability
// signal. Rust rewrite of pr-review-report.sh, fixing the 16 bugs from the adversarial review.
//
// Usage:   pr-review-report            # all buckets
//          pr-review-report --ready    # only the reviewed-&-ready-to-merge bucket
//          pr-review-report --queue [N]                 # cheapest-first review queue
//          pr-review-report --commit-closes <owner/repo> <pr>  # fail if a commit keyword closes an out-of-index issue
// Config (env overrides cron.env in CWD, then default): ORG, PR_ASSIGNEE, CLOSE_CANDIDATES, REVIEW_VERDICTS.

use serde_json::Value;
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Ci {
    Red,
    Pending,
    NoChecks,
    Green,
}

#[derive(Clone, Copy, PartialEq)]
enum Merge {
    Mergeable,
    Conflicting,
    Unknown,
}
/// Run gh and parse stdout as JSON; None on non-zero exit, spawn failure, or unparseable output.
fn gh_json(args: &[&str]) -> Option<Value> {
    let out = Command::new("gh").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Run gh for a WRITE that returns no JSON (label/comment/edit); true on success. The seam that keeps
/// `--record-verdict`'s logic testable without network.
fn gh_run(args: &[&str]) -> bool {
    Command::new("gh")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// FIX(bug 2): a CheckRun is pending unless status==COMPLETED (WAITING/REQUESTED/QUEUED/IN_PROGRESS
/// all count as pending); a StatusContext is pending unless its state is terminal (SUCCESS/FAILURE/
/// ERROR) — so EXPECTED/PENDING count as pending. A not-yet-concluded check is never GREEN.
fn classify_ci(rollup: &Value) -> Ci {
    let empty = Vec::new();
    let arr = rollup.as_array().unwrap_or(&empty);
    let mut fail = 0usize;
    let mut pend = 0usize;
    let tot = arr.len();
    for it in arr {
        let concl = it.get("conclusion").and_then(|v| v.as_str());
        let state = it.get("state").and_then(|v| v.as_str());
        let status = it.get("status").and_then(|v| v.as_str());
        let is_fail = matches!(
            concl,
            Some("FAILURE")
                | Some("TIMED_OUT")
                | Some("CANCELLED")
                | Some("ACTION_REQUIRED")
                | Some("STARTUP_FAILURE")
        ) || matches!(state, Some("FAILURE") | Some("ERROR"));
        if is_fail {
            fail += 1;
            continue;
        }
        let is_pend = if let Some(st) = status {
            st != "COMPLETED"
        } else if let Some(s) = state {
            !matches!(s, "SUCCESS" | "FAILURE" | "ERROR")
        } else {
            // FIX(rs-bug 3): a check with neither status nor state is unconfirmed → pending, never green.
            true
        };
        if is_pend {
            pend += 1;
        }
    }
    if fail > 0 {
        Ci::Red
    } else if pend > 0 {
        Ci::Pending
    } else if tot == 0 {
        Ci::NoChecks
    } else {
        Ci::Green
    }
}
/// One queue row for cheapest-first display: (cost, repo-display, number, url, basis). Unscored
/// rows carry cost 1001 so they sort last.
type QueueRow = (i64, String, u64, String, String);

#[derive(Clone, Copy, PartialEq, Debug)]
enum PresentState {
    Presentable,
    Red,
    Pending,
    Conflicting,
    MergeUnknown,
    Approved,
}

/// Pure: is an `ai:ready`-labelled PR presentable for a human decision right now?
/// A PR a human has already APPROVED has left the pending-review queue; red or pending CI, a merge
/// conflict, and UNCONFIRMED mergeability are each disqualifying; only green (or no configured
/// checks) + CONFIRMED-mergeable is presentable — the human sees only fully-clean PRs.
fn presentable_state(ci: Ci, merge: Merge, review_decision: Option<&str>) -> PresentState {
    if review_decision == Some("APPROVED") {
        return PresentState::Approved;
    }
    match ci {
        Ci::Red => PresentState::Red,
        Ci::Pending => PresentState::Pending,
        Ci::Green | Ci::NoChecks => match merge {
            Merge::Conflicting => PresentState::Conflicting,
            // Unknown = GitHub has not confirmed the PR merges cleanly. Not fully clean, so not
            // presentable; surfaced as MergeUnknown (the producer's job to settle before a human views).
            Merge::Unknown => PresentState::MergeUnknown,
            Merge::Mergeable => PresentState::Presentable,
        },
    }
}
/// A `gh search` result carries a human override label (which beats an `ai:ready` label) when any
/// of its labels is `human:reject` / `human:design` / `human:close-candidate`.
fn has_human_override(p: &Value) -> bool {
    p.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter().any(|l| {
                matches!(
                    l.get("name").and_then(|n| n.as_str()),
                    Some("human:reject") | Some("human:design") | Some("human:close-candidate")
                )
            })
        })
        .unwrap_or(false)
}

/// A native GitHub human review (`reviewDecision` APPROVED or CHANGES_REQUESTED) is a human decision
/// too, as sacred as a `human:*` label. Checked at WRITE time so a review that lands between the
/// vetter's read and its record cannot be clobbered — this closes the human-review TOCTOU race.
fn has_native_human_review(p: &Value) -> bool {
    matches!(
        p.get("reviewDecision").and_then(|d| d.as_str()),
        Some("APPROVED") | Some("CHANGES_REQUESTED")
    )
}

/// owner/repo slug from a GitHub PR url — the search result's own URL, never guessed by org.
/// None for anything that is not an https://github.com/<owner>/<repo>/pull/<n> URL.
fn pr_slug(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let slug = rest.split("/pull/").next()?;
    if slug.is_empty() || !slug.contains('/') || !rest.contains("/pull/") {
        return None;
    }
    Some(slug.to_string())
}

/// Aggregate queue counts for the header (see `render_queue`).
struct QueueCounts {
    raw: usize,      // all ai:ready PRs the search returned
    excluded: usize, // filtered before the per-PR check: drafts + human:* overrides
    conflict: usize,
    red: usize,
    pending: usize,
    merge_unknown: usize,
    approved: usize,
    unconfirmed: usize, // green+mergeable but no ai:vetter comment at head — awaiting (re-)vet, not shown
    fetch_error: usize,
}

/// Render the queue: a header with the true ai:ready -> presentable / conflicting / red / pending /
/// approved breakdown, then the cheapest-first presentable rows (printed list capped at `top`,
/// 0 = all; a `+N more` line notes any presentable rows beyond the cap).
fn render_queue(rows: &[QueueRow], c: &QueueCounts, top: usize) -> String {
    let trunc = if c.raw >= 1000 {
        "  [WARNING: search hit the 1000-result limit — queue may be undercounted]"
    } else {
        ""
    };
    let err = if c.fetch_error > 0 {
        format!(", {} fetch-error", c.fetch_error)
    } else {
        String::new()
    };
    let excl = if c.excluded > 0 {
        format!(", {} excluded (draft/human-override)", c.excluded)
    } else {
        String::new()
    };
    let shown = if top == 0 {
        rows.len()
    } else {
        top.min(rows.len())
    };
    let mut out = format!(
        "review queue: {} ai:ready -> {} presentable, {} conflicting, {} red, {} pending, {} unknown-merge, {} approved, {} awaiting re-vet{}{} (cheapest first){}\n",
        c.raw, rows.len(), c.conflict, c.red, c.pending, c.merge_unknown, c.approved, c.unconfirmed, err, excl, trunc
    );
    for (cost, repo, num, url, basis) in rows.iter().take(shown) {
        let cs = if *cost == 1001 {
            "unscored".to_string()
        } else {
            format!("{cost:>4}")
        };
        out.push_str(&format!("\n  {cs}  {repo}#{num}  {basis}\n        {url}"));
    }
    if rows.len() > shown {
        out.push_str(&format!("\n  … +{} more presentable", rows.len() - shown));
    }
    out
}

fn queue_mode(top: usize) {
    // Candidates come from the `ai:ready` LABEL, NOT `gh search --checks success`. That qualifier is
    // unreliable — the identical query returned 93 then 203 open PRs minutes apart, which collapsed a
    // 75-deep review queue to "1". Label search is reliable; CI/mergeability is then verified per-PR
    // below (statusCheckRollup + mergeable), never trusted from the search layer.
    let Some(val) = gh_json(&[
        "search",
        "prs",
        "--owner",
        "rainlanguage",
        "--owner",
        "cyclofinance",
        "--state",
        "open",
        "--label",
        "ai:ready",
        "--limit",
        "1000",
        "--json",
        "url,number,repository,isDraft,labels",
    ]) else {
        eprintln!("error: `gh search prs --label ai:ready` failed (transient API error / auth?) — aborting rather than print a falsely-empty queue");
        std::process::exit(1);
    };
    let Some(arr) = val.as_array() else {
        eprintln!("error: `gh search prs` returned non-array JSON — aborting");
        std::process::exit(1);
    };

    // Candidate filter (from the search JSON, no extra call): drop drafts and any PR whose ai:ready
    // is overridden by a human:* label (the human's verdict wins).
    let candidates: Vec<(String, u64, String)> = arr
        .iter()
        .filter(|p| !p.get("isDraft").and_then(|x| x.as_bool()).unwrap_or(false))
        .filter(|p| !has_human_override(p))
        .filter_map(|p| {
            let num = p.get("number").and_then(|n| n.as_u64())?;
            let url = p
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let slug = pr_slug(&url)?;
            Some((slug, num, url))
        })
        .collect();

    // Full per-PR pass over every candidate — after the 1-vs-75 failure, an ACCURATE queue is the
    // whole point, so each candidate's real CI rollup + mergeable + reviewDecision is fetched.
    let mut rows: Vec<QueueRow> = Vec::new();
    let mut counts = QueueCounts {
        raw: arr.len(),
        excluded: arr.len() - candidates.len(),
        conflict: 0,
        red: 0,
        pending: 0,
        merge_unknown: 0,
        approved: 0,
        unconfirmed: 0,
        fetch_error: 0,
    };
    for (slug, num, url) in &candidates {
        let Some(j) = gh_json(&[
            "pr",
            "view",
            &num.to_string(),
            "-R",
            slug,
            "--json",
            "mergeable,statusCheckRollup,reviewDecision,headRefOid,comments",
        ]) else {
            counts.fetch_error += 1;
            continue;
        };
        let merge = match j.get("mergeable").and_then(|x| x.as_str()) {
            Some("MERGEABLE") => Merge::Mergeable,
            Some("CONFLICTING") => Merge::Conflicting,
            _ => Merge::Unknown,
        };
        let ci = classify_ci(j.get("statusCheckRollup").unwrap_or(&Value::Null));
        let rev = j
            .get("reviewDecision")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty());
        match presentable_state(ci, merge, rev) {
            PresentState::Presentable => {
                // Vetted-at-head gate: green + mergeable is not enough — the ai:ready label must be
                // BACKED by an ai:vetter comment at the current head. A migration-labelled or
                // pushed-since PR is not presented; it's counted as awaiting (re-)vet.
                let head = j.get("headRefOid").and_then(|x| x.as_str()).unwrap_or("");
                if vetted_at_head(&j, head) {
                    let (cost, basis) = cost_from_comment(last_vetter_comment(&j).as_deref());
                    let repo_disp = slug.rsplit('/').next().unwrap_or(slug).to_string();
                    rows.push((cost, repo_disp, *num, url.clone(), basis));
                } else {
                    counts.unconfirmed += 1;
                }
            }
            PresentState::Red => counts.red += 1,
            PresentState::Pending => counts.pending += 1,
            PresentState::Conflicting => counts.conflict += 1,
            PresentState::MergeUnknown => counts.merge_unknown += 1,
            PresentState::Approved => counts.approved += 1,
        }
    }
    rows.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
    println!("{}", render_queue(&rows, &counts, top));
}
/// Parse the closing-keyword issue numbers from arbitrary text (a commit message or a
/// PR body). Matches GitHub's own set — close/closes/closed, fix/fixes/fixed,
/// resolve/resolves/resolved — followed by optional whitespace and `#N`, case-insensitively.
/// GitHub requires the keyword IMMEDIATELY before the `#N` (a keyword and a bare `#N`
/// elsewhere in the same text do NOT link), so this matches `<keyword>[ :]#N` adjacency,
/// not a keyword anywhere plus a `#N` anywhere. Returns the numbers in first-seen order,
/// de-duplicated.
fn closing_keywords(text: &str) -> Vec<u64> {
    const KEYWORDS: &[&str] = &[
        "closes", "closed", "close", "fixes", "fixed", "fix", "resolves", "resolved", "resolve",
    ];
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut out: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // find the next keyword whose start is at a word boundary
        let at_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if at_boundary {
            if let Some(kw) = KEYWORDS.iter().find(|kw| lower[i..].starts_with(**kw)) {
                let mut j = i + kw.len();
                // No separate "keyword is a word-prefix" guard is needed: a keyword that only
                // prefixes a longer word (`closest`) is followed by a letter, which is not a
                // separator, so the `#`-adjacency check below rejects it anyway.
                // skip a single optional separator run of spaces/colon between keyword and #
                while bytes
                    .get(j)
                    .map(|c| *c == b' ' || *c == b':' || *c == b'\t')
                    .unwrap_or(false)
                {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'#') {
                    j += 1;
                    let start = j;
                    while bytes.get(j).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        j += 1;
                    }
                    if j > start {
                        if let Ok(n) = lower[start..j].parse::<u64>() {
                            if !out.contains(&n) {
                                out.push(n);
                            }
                        }
                        i = j;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// `--commit-closes <owner/repo> <pr>`: fail (exit 1) if any closing keyword in a branch
/// COMMIT MESSAGE references an issue that is NOT in the PR's live closingIssuesReferences.
/// Commit-message keywords fire on merge independently of the PR body, so a body relink does
/// not neutralize them — this catches the erc4626#217 auto-close class before merge.
fn commit_closes_mode(slug: &str, pr: &str) -> i32 {
    let Some(commits) = gh_json(&["pr", "view", pr, "-R", slug, "--json", "commits"]) else {
        eprintln!("error: could not fetch commits for {slug}#{pr}");
        return 2;
    };
    let mut kw: Vec<u64> = Vec::new();
    if let Some(arr) = commits.get("commits").and_then(|c| c.as_array()) {
        for c in arr {
            let head = c
                .get("messageHeadline")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let body = c.get("messageBody").and_then(|x| x.as_str()).unwrap_or("");
            for n in closing_keywords(&format!("{head}\n{body}")) {
                if !kw.contains(&n) {
                    kw.push(n);
                }
            }
        }
    }
    let Some(refs) = gh_json(&[
        "pr",
        "view",
        pr,
        "-R",
        slug,
        "--json",
        "closingIssuesReferences",
    ]) else {
        eprintln!("error: could not fetch closingIssuesReferences for {slug}#{pr}");
        return 2;
    };
    let indexed: Vec<u64> = refs
        .get("closingIssuesReferences")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("number").and_then(|n| n.as_u64()))
                .collect()
        })
        .unwrap_or_default();
    let extras: Vec<u64> = kw
        .iter()
        .copied()
        .filter(|n| !indexed.contains(n))
        .collect();
    if extras.is_empty() {
        println!("commit-closes {slug}#{pr}: OK (commit keywords {kw:?} all in index {indexed:?})");
        0
    } else {
        println!(
            "commit-closes {slug}#{pr}: MISMATCH — commit messages close {extras:?} not in the PR's closing index {indexed:?}; these auto-close on merge regardless of the body. Rewrite history or accept the closes before merging."
        );
        1
    }
}

/// Metrics extracted from one claude run trace (a stream-json `.jsonl`). Startup overhead
/// is measured in TOOL CALLS (always present) — the count of tool calls before the run's
/// first org-mutating action — because state recovery (issue/PR enumeration, dedup) runs as
/// read-only tool calls before any PR/issue/commit is created.
#[derive(Default, PartialEq, Debug)]
struct RunMetrics {
    tool_calls: usize,
    startup_tool_calls: usize,
    // ScheduleWakeup / CronCreate calls. A one-shot cron must NEVER park itself to resume "later";
    // any non-zero value is a regression of the no-park rule (both tools are denied in settings).
    wakeup_calls: usize,
    first_mutation_index: Option<usize>,
    duration_ms: u64,
    num_turns: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_creation: u64,
    cost_usd: f64,
}

impl RunMetrics {
    fn startup_pct(&self) -> f64 {
        if self.tool_calls == 0 {
            0.0
        } else {
            (self.startup_tool_calls as f64 / self.tool_calls as f64) * 100.0
        }
    }
}

/// A tool call is an org MUTATION when it is a Bash command that creates/edits/merges/closes
/// a PR or issue, or commits/pushes — i.e. the run stopped recovering state and started doing
/// work. Read-only gh/git/grep calls are NOT mutations.
fn is_mutation_tool(name: &str, input: &serde_json::Value) -> bool {
    if name != "Bash" {
        return false;
    }
    let cmd = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
    const MARKERS: &[&str] = &[
        "pr create",
        "pr comment",
        "pr merge",
        "pr edit",
        "pr close",
        "pr ready",
        "issue create",
        "issue comment",
        "issue close",
        "issue reopen",
        "issue edit",
        "git commit",
        "git push",
        "git merge",
    ];
    MARKERS.iter().any(|m| cmd.contains(m))
}

/// Parse a stream-json trace: count tool calls in order, find the first mutation, and take
/// the usage/duration/cost from the result event with the most turns (the main run — trailing
/// short result events from continuations are ignored).
fn run_metrics(content: &str) -> RunMetrics {
    let mut m = RunMetrics::default();
    let mut best_turns = 0u64;
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(content) = v
                    .get("message")
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            let empty = serde_json::json!({});
                            let input = block.get("input").unwrap_or(&empty);
                            if name == "ScheduleWakeup" || name == "CronCreate" {
                                m.wakeup_calls += 1;
                            }
                            if m.first_mutation_index.is_none() {
                                if is_mutation_tool(name, input) {
                                    m.first_mutation_index = Some(m.tool_calls);
                                } else {
                                    m.startup_tool_calls += 1;
                                }
                            }
                            m.tool_calls += 1;
                        }
                    }
                }
            }
            Some("result") => {
                let turns = v.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0);
                if turns >= best_turns {
                    best_turns = turns;
                    m.num_turns = turns;
                    m.duration_ms = v.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0);
                    m.cost_usd = v
                        .get("total_cost_usd")
                        .and_then(|c| c.as_f64())
                        .unwrap_or(0.0);
                    let u = v.get("usage");
                    let g = |k: &str| {
                        u.and_then(|u| u.get(k))
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0)
                    };
                    m.tokens_in = g("input_tokens");
                    m.tokens_out = g("output_tokens");
                    m.cache_read = g("cache_read_input_tokens");
                    m.cache_creation = g("cache_creation_input_tokens");
                }
            }
            _ => {}
        }
    }
    m
}

/// `--run-metrics <trace.jsonl>`: print the run's metrics (startup overhead, duration, tokens,
/// cost) as one JSON line — the input to a committed metrics/runs.jsonl and the #7 dashboard.
fn run_metrics_mode(path: &str) -> i32 {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read trace {path}: {e}");
            return 2;
        }
    };
    let m = run_metrics(&content);
    let doc = serde_json::json!({
        "trace": path,
        "toolCalls": m.tool_calls,
        "startupToolCalls": m.startup_tool_calls,
        "startupPct": (m.startup_pct() * 10.0).round() / 10.0,
        "wakeupCalls": m.wakeup_calls,
        "firstMutationIndex": m.first_mutation_index,
        "durationMs": m.duration_ms,
        "numTurns": m.num_turns,
        "tokensIn": m.tokens_in,
        "tokensOut": m.tokens_out,
        "cacheRead": m.cache_read,
        "cacheCreation": m.cache_creation,
        "costUsd": (m.cost_usd * 1000.0).round() / 1000.0,
    });
    println!("{}", serde_json::to_string(&doc).unwrap());
    0
}

/// verdict word -> the `ai:*` label it records. None for anything else.
fn verdict_label(verdict: &str) -> Option<&'static str> {
    match verdict {
        "ready" => Some("ai:ready"),
        "reject" => Some("ai:reject"),
        "design" => Some("ai:design"),
        "close" => Some("ai:close-candidate"),
        "relink" => Some("ai:relink"),
        _ => None,
    }
}

/// GitHub colour + description for an `ai:*` verdict label (matches the taxonomy already created
/// across the repos).
fn label_meta(label: &str) -> (&'static str, &'static str) {
    match label {
        "ai:ready" => (
            "0e8a16",
            "AI vetter: passes review, ready for human decision",
        ),
        "ai:reject" => ("b60205", "AI vetter: needs rework (code issue)"),
        "ai:design" => ("5319e7", "AI vetter: raises a design question"),
        "ai:close-candidate" => ("c5def5", "AI vetter: candidate to close"),
        "ai:relink" => (
            "fbca04",
            "AI vetter: sound code, needs Closes→Refs linkage fix",
        ),
        _ => ("cccccc", "AI vetter verdict"),
    }
}

/// The `ai:*` labels to strip so the PR ends with exactly ONE AI verdict: every `ai:*` label present
/// EXCEPT the target. `human:*` and non-`ai:` labels are left untouched.
fn labels_to_remove(current: &[String], target: &str) -> Vec<String> {
    current
        .iter()
        .filter(|l| l.starts_with("ai:") && l.as_str() != target)
        .cloned()
        .collect()
}

/// The SHA-bound vetter comment: `🤖 ai:vetter` marker line, then `Reviewed <sha>: <verdict>` (plus
/// ` — <note>`), then a `cost <n> — <basis>` line when a cost is given. The cost is on its OWN line so
/// the `Reviewed <sha>:`/`Reviewed <sha>: <verdict>` matches (vetted-at-head, skip-dedup) are unaffected.
/// This comment is now the SOLE home of verification cost — there is no cost sidecar.
fn verdict_comment(sha: &str, verdict: &str, note: &str, cost: Option<i64>, basis: &str) -> String {
    let tail = if note.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", note.trim())
    };
    let cost_line = match cost {
        Some(c) if basis.trim().is_empty() => format!("\ncost {c}"),
        Some(c) => format!("\ncost {c} — {}", basis.trim()),
        None => String::new(),
    };
    format!("🤖 ai:vetter\nReviewed {sha}: {verdict}{tail}{cost_line}")
}

/// Verification cost + basis parsed from a vetter comment's `cost <n> — <basis>` line, else
/// (1001, "") = unscored (sorts last). This is where the queue reads cost now that the sidecar is gone.
fn cost_from_comment(body: Option<&str>) -> (i64, String) {
    let Some(body) = body else {
        return (1001, String::new());
    };
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("cost ") {
            let (num, basis) = match rest.split_once(" — ") {
                Some((n, b)) => (n.trim(), b.trim()),
                None => (rest.trim(), ""),
            };
            if let Ok(c) = num.parse::<i64>() {
                return (c, basis.to_string());
            }
        }
    }
    (1001, String::new())
}
/// The GitHub login the pipeline's shared bot account authenticates as — the human, the producer
/// cron, and the vetter cron ALL post as this one account, disambiguated only by role markers
/// (`🤖 ai:vetter`, `🤖 ai:producer`, "Rework note"). It is the ONLY author whose comments the tooling
/// trusts as authoritative: every marker is public body text any third party can post, so a
/// trust-bearing comment is authenticated by AUTHOR, never by marker alone. Change it here if that
/// identity ever moves (e.g. to a dedicated bot account).
const TRUSTED_AUTHOR: &str = "thedavidmeister";

/// `author.login` of a comment `Value`, if present.
fn author_login(comment: &Value) -> Option<&str> {
    comment
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(|l| l.as_str())
}

/// Bodies of the PR/issue comments authored by [`TRUSTED_AUTHOR`], in chronological order, optionally
/// restricted to those whose body starts with `marker`. The author filter is the provenance guard —
/// it drops any spoofed comment carrying a role marker from a different account; `marker` merely
/// selects which trusted role's comments (vetter / producer / …) you want. This is the single choke
/// point every trust-bearing comment read goes through.
fn trusted_comments(pr: &Value, marker: Option<&str>) -> Vec<String> {
    pr.get("comments")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter(|c| author_login(c) == Some(TRUSTED_AUTHOR))
        .filter_map(|c| c.get("body").and_then(|b| b.as_str()))
        .filter(|b| marker.is_none_or(|m| b.starts_with(m)))
        .map(String::from)
        .collect()
}

/// The most-recent trusted `🤖 ai:vetter` comment body (the queue / record-verdict provenance
/// anchor), or None. A spoofed marker from an untrusted author is ignored — see [`trusted_comments`].
fn last_vetter_comment(pr: &Value) -> Option<String> {
    trusted_comments(pr, Some("🤖 ai:vetter")).pop()
}

/// A PR is vetted AT HEAD only when its most-recent `🤖 ai:vetter` comment recorded a verdict at the
/// CURRENT head sha (`Reviewed <head>:`). The `ai:*` label alone can be stale — migration-applied, or
/// from before the producer pushed a commit — so the queue uses this stricter bar (the vetter's own
/// definition) to never present a PR whose AI verdict isn't confirmed against the exact commit.
fn vetted_at_head(pr_json: &Value, head: &str) -> bool {
    !head.is_empty()
        && last_vetter_comment(pr_json)
            .map(|b| b.contains(&format!("Reviewed {head}:")))
            .unwrap_or(false)
}

/// Skip a new vetter comment iff the last one already recorded the SAME verdict at the SAME head sha
/// (no-op re-review). A moved head or a changed verdict does NOT skip.
fn should_skip_comment(last_vetter_body: Option<&str>, sha: &str, verdict: &str) -> bool {
    match last_vetter_body {
        Some(b) => b.contains(&format!("Reviewed {sha}: {verdict}")),
        None => false,
    }
}

/// The recording decision, computed PURELY from the fetched PR JSON so the guard-before-write logic
/// is unit-testable (not just the leaf helpers): refuse if a human verdict is present, refuse if
/// there is no head sha, else the label plan + whether the comment is a dedup no-op.
#[derive(PartialEq, Debug)]
enum VerdictPlan {
    RefuseHuman,
    NoSha,
    Record {
        to_remove: Vec<String>,
        has_target: bool,
        sha: String,
        skip_comment: bool,
    },
}

fn verdict_plan(pr_json: &Value, target: &str, verdict: &str) -> VerdictPlan {
    // Sacred: never override a human verdict — a human:* label OR a native GitHub review
    // (APPROVED/CHANGES_REQUESTED). This is the guard whose ABSENCE a mutation must fail.
    if has_human_override(pr_json) || has_native_human_review(pr_json) {
        return VerdictPlan::RefuseHuman;
    }
    let sha = pr_json
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // No head sha ⇒ can't write a SHA-bound verdict; refuse rather than post "Reviewed :".
    if sha.is_empty() {
        return VerdictPlan::NoSha;
    }
    let current: Vec<String> = pr_json
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let to_remove = labels_to_remove(&current, target);
    let has_target = current.iter().any(|c| c == target);
    let skip_comment = should_skip_comment(last_vetter_comment(pr_json).as_deref(), sha, verdict);
    VerdictPlan::Record {
        to_remove,
        has_target,
        sha: sha.to_string(),
        skip_comment,
    }
}

/// `--record-verdict <owner/repo> <pr> <verdict> [note...]`: record an AI verdict as the
/// `ai:<verdict>` label (exactly one AI verdict at a time) + a SHA-bound `🤖 ai:vetter` comment.
/// The ONE writer of AI verdicts (shared by the vetter); never overrides a human verdict.
#[allow(clippy::too_many_arguments)]
fn record_verdict_mode(
    slug: &str,
    pr: &str,
    verdict: &str,
    note: &str,
    cost: Option<i64>,
    basis: &str,
    dry_run: bool,
) -> i32 {
    let Some(target) = verdict_label(verdict) else {
        eprintln!(
            "usage: pr-review-report --record-verdict <owner/repo> <pr> <ready|reject|design|close|relink> [note...] [--cost <n>] [--basis <s>] [--dry-run]"
        );
        return 2;
    };
    let Some(pr_json) = gh_json(&[
        "pr",
        "view",
        pr,
        "-R",
        slug,
        "--json",
        "headRefOid,labels,comments,reviewDecision",
    ]) else {
        eprintln!("error: `gh pr view {slug}#{pr}` failed — not writing on incomplete data");
        return 1;
    };
    let (to_remove, has_target, sha, skip) = match verdict_plan(&pr_json, target, verdict) {
        VerdictPlan::RefuseHuman => {
            eprintln!("human verdict present on {slug}#{pr}; not overriding");
            return 3;
        }
        VerdictPlan::NoSha => {
            eprintln!(
                "error: {slug}#{pr} has no head sha (headRefOid) — not recording a verdict without one"
            );
            return 1;
        }
        VerdictPlan::Record {
            to_remove,
            has_target,
            sha,
            skip_comment,
        } => (to_remove, has_target, sha, skip_comment),
    };
    let comment = verdict_comment(&sha, verdict, note, cost, basis);

    if dry_run {
        println!("[dry-run] {slug}#{pr} @ {sha}");
        println!(
            "  target label: {target}{}",
            if has_target { " (already present)" } else { "" }
        );
        println!(
            "  labels to remove: {}",
            if to_remove.is_empty() {
                "(none)".to_string()
            } else {
                to_remove.join(", ")
            }
        );
        println!(
            "  comment: {}",
            if skip {
                "skip (same verdict + sha already posted)".to_string()
            } else {
                format!("post -> {}", comment.replace('\n', " / "))
            }
        );
        println!(
            "  cost: {}",
            match cost {
                Some(c) => format!("{c} ({basis}) -> embedded in the comment"),
                None => "(none)".to_string(),
            }
        );
        return 0;
    }

    let (color, desc) = label_meta(target);
    if !gh_run(&[
        "label",
        "create",
        target,
        "-R",
        slug,
        "--color",
        color,
        "--description",
        desc,
        "--force",
    ]) {
        eprintln!("warning: could not ensure label {target} exists in {slug}");
    }
    if !has_target && !gh_run(&["pr", "edit", pr, "-R", slug, "--add-label", target]) {
        eprintln!("error: failed to add {target} to {slug}#{pr}");
        return 1;
    }
    for r in &to_remove {
        if !gh_run(&["pr", "edit", pr, "-R", slug, "--remove-label", r]) {
            eprintln!("warning: failed to remove label {r} from {slug}#{pr}");
        }
    }
    // A swallowed comment failure would report success with the SHA-bound rationale never posted.
    // The cost now travels INSIDE this comment (verdict_comment embeds it) — there is no cost sidecar.
    if !skip && !gh_run(&["pr", "comment", pr, "-R", slug, "--body", &comment]) {
        eprintln!("error: recorded {target} on {slug}#{pr} but FAILED to post the verdict comment");
        return 1;
    }
    println!(
        "recorded {target} on {slug}#{pr}{}{}{}",
        if to_remove.is_empty() {
            String::new()
        } else {
            format!(" (removed {})", to_remove.join(","))
        },
        if skip {
            " [comment deduped]"
        } else {
            " [comment posted]"
        },
        match cost {
            Some(c) => format!(" [cost {c}]"),
            None => String::new(),
        }
    );
    0
}

/// `--trusted-comments`: print the comments on a PR (or issue, with `--issue`) authored by the
/// trusted account, most-recent last, separated by a `---` line, optionally filtered to a `--marker`
/// body prefix. Exit 0 if any trusted comment matched, 1 if none (so a caller can branch on "have I
/// already posted this?"), 2 on fetch error. This is the ONLY sanctioned way for the producer to read
/// a comment as authoritative (rework notes, its own hand-off / screenshot markers): hand-reading
/// `gh pr view --comments` trusts spoofable body text, this authenticates by author first.
fn trusted_comments_mode(slug: &str, n: &str, marker: Option<&str>, issue: bool) -> i32 {
    let kind = if issue { "issue" } else { "pr" };
    let Some(j) = gh_json(&[kind, "view", n, "-R", slug, "--json", "comments"]) else {
        eprintln!("error: could not fetch comments for {slug}#{n}");
        return 2;
    };
    let bodies = trusted_comments(&j, marker);
    for (i, b) in bodies.iter().enumerate() {
        if i > 0 {
            println!("---");
        }
        println!("{b}");
    }
    i32::from(bodies.is_empty())
}

/// PR state as reported by `gh pr list`, for the clone-gc decision.
#[derive(Debug, PartialEq, Eq)]
enum PrState {
    Open,
    Merged,
    Closed,
}

/// What gc should do with one clone, plus a human-readable reason.
#[derive(Debug, PartialEq, Eq)]
enum GcAction {
    Delete(String),
    Keep(String),
}

/// One clone's state, as gathered for the gc decision.
struct CloneState {
    /// No uncommitted changes (`git status --porcelain` empty).
    clean: bool,
    /// Commits present locally but on NO remote-tracking branch — i.e. unpushed work. `None` when it
    /// could not be determined (a git error), which is treated as possibly-unpushed → keep (fail safe).
    unpushed: Option<u32>,
    /// Resolved PR state for the checked-out branch, if any.
    pr: Option<PrState>,
    /// Days since the clone was last modified.
    age_days: u64,
}

/// Map a `gh pr list` state string to a [`PrState`].
fn parse_pr_state(s: &str) -> Option<PrState> {
    match s {
        "OPEN" => Some(PrState::Open),
        "MERGED" => Some(PrState::Merged),
        "CLOSED" => Some(PrState::Closed),
        _ => None,
    }
}

/// Extract `owner/repo` from a git remote URL (https or ssh form), stripping a trailing `.git`.
/// `https://github.com/rainlanguage/raindex.git` → `rainlanguage/raindex`;
/// `git@github.com:rainlanguage/cyclo.site.git`  → `rainlanguage/cyclo.site` (dots in the repo name
/// are preserved — only a trailing `.git` is stripped).
fn parse_repo_slug(remote_url: &str) -> Option<String> {
    let (_, rest) = remote_url.trim().split_once("github.com")?;
    let rest = rest.trim_start_matches([':', '/']);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut it = rest.split('/');
    let owner = it.next().filter(|x| !x.is_empty())?;
    let repo = it.next().filter(|x| !x.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// Decide whether a clone is safe to garbage-collect, with a reason. Precedence is deliberate:
/// unpushed/uncommitted work is ALWAYS preserved (never gc'd, whatever the PR state); then a
/// merged/closed PR means the work has landed or been abandoned upstream, so the clone is disposable;
/// an open PR is active work (kept); a clone with no resolvable PR is kept until it goes stale (the
/// age backstop) so ad-hoc clones with no PR don't accumulate forever.
fn gc_decision(s: &CloneState, max_age_days: u64) -> GcAction {
    if !s.clean {
        return GcAction::Keep("uncommitted changes".into());
    }
    // Fail SAFE: `None` means the unpushed count couldn't be computed (e.g. no upstream), so we can't
    // prove the work is pushed — never delete it. Some(>0) is genuinely unpushed work — also keep.
    match s.unpushed {
        None => return GcAction::Keep("unpushed state unknown".into()),
        Some(n) if n > 0 => return GcAction::Keep(format!("{n} unpushed commit(s)")),
        Some(_) => {}
    }
    match s.pr {
        Some(PrState::Merged) => GcAction::Delete("PR merged".into()),
        Some(PrState::Closed) => GcAction::Delete("PR closed".into()),
        Some(PrState::Open) => GcAction::Keep("open PR".into()),
        None => {
            if s.age_days >= max_age_days {
                GcAction::Delete(format!("no PR, idle {}d", s.age_days))
            } else {
                GcAction::Keep(format!("no PR, idle {}d < {max_age_days}d", s.age_days))
            }
        }
    }
}

/// Run `git -C <dir> <args>` and return trimmed stdout, or None on spawn failure / non-zero exit.
fn git_out(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve the PR state of the clone's checked-out branch, or None when there's no PR (or it can't be
/// resolved — detached HEAD, missing remote, offline). Only the first `gh pr list` match is used.
fn resolve_pr_state(dir: &std::path::Path) -> Option<PrState> {
    let branch = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch.is_empty() || branch == "HEAD" {
        return None; // detached HEAD — nothing to map
    }
    let slug = parse_repo_slug(&git_out(dir, &["remote", "get-url", "origin"])?)?;
    let v = gh_json(&[
        "pr", "list", "-R", &slug, "--head", &branch, "--state", "all", "--json", "state",
        "--limit", "1",
    ])?;
    parse_pr_state(v.as_array()?.first()?.get("state")?.as_str()?)
}

/// Days since the clone dir was last modified (0 on any error — errs toward KEEPING, since only the
/// no-PR age backstop consults it).
fn clone_age_days(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// `--gc-clones <work-dir> [--dry-run] [--max-age-days N]`: garbage-collect the per-PR/issue work
/// clones directly under <work-dir>. A clone is deleted only when it is clean + fully pushed AND its
/// checked-out branch's PR is merged/closed (or it has no PR and has been idle past the age cap);
/// clones with uncommitted/unpushed work or an open PR are always kept. Prints one line per clone.
fn gc_clones_mode(work_dir: &str, max_age_days: u64, dry_run: bool) -> i32 {
    let Ok(entries) = std::fs::read_dir(work_dir) else {
        eprintln!("error: cannot read work-dir {work_dir}");
        return 2;
    };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(".git").is_dir())
        .collect();
    dirs.sort();
    let (mut deleted, mut kept) = (0u32, 0u32);
    for dir in &dirs {
        let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        let clean = git_out(dir, &["status", "--porcelain"])
            .map(|s| s.is_empty())
            .unwrap_or(false);
        // Unpushed commits = on HEAD but on NO remote-tracking branch. This works WITHOUT a configured
        // upstream (unlike `@{u}..HEAD`, which errors on an upstream-less branch); a git error stays
        // `None` (not 0) so gc_decision fails safe and keeps a clone whose push-state is unknown.
        let unpushed = git_out(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
            .and_then(|s| s.parse::<u32>().ok());
        let state = CloneState {
            clean,
            unpushed,
            pr: resolve_pr_state(dir),
            age_days: clone_age_days(dir),
        };
        match gc_decision(&state, max_age_days) {
            GcAction::Delete(reason) => {
                if dry_run {
                    println!("would delete  {name}  ({reason})");
                    deleted += 1;
                } else if std::fs::remove_dir_all(dir).is_ok() {
                    println!("deleted       {name}  ({reason})");
                    deleted += 1;
                } else {
                    eprintln!("error deleting {name}");
                    kept += 1;
                }
            }
            GcAction::Keep(reason) => {
                println!("kept          {name}  ({reason})");
                kept += 1;
            }
        }
    }
    let verb = if dry_run { "would gc" } else { "gc" };
    println!(
        "{verb}: {deleted} deleted, {kept} kept ({} clones)",
        dirs.len()
    );
    0
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--queue") {
        let top = args
            .get(2)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);
        queue_mode(top);
        return;
    }
    if args.get(1).map(String::as_str) == Some("--commit-closes") {
        let (Some(slug), Some(pr)) = (args.get(2), args.get(3)) else {
            eprintln!("usage: pr-review-report --commit-closes <owner/repo> <pr>");
            std::process::exit(2);
        };
        std::process::exit(commit_closes_mode(slug, pr));
    }
    if args.get(1).map(String::as_str) == Some("--trusted-comments") {
        let rest = &args[2..];
        let issue = rest.iter().any(|a| a == "--issue");
        let mut marker: Option<&str> = None;
        let mut positional: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--issue" => {}
                "--marker" => {
                    i += 1;
                    marker = rest.get(i).map(String::as_str);
                }
                other => positional.push(other),
            }
            i += 1;
        }
        let (Some(&slug), Some(&n)) = (positional.first(), positional.get(1)) else {
            eprintln!(
                "usage: pr-review-report --trusted-comments <owner/repo> <n> [--marker <prefix>] [--issue]"
            );
            std::process::exit(2);
        };
        std::process::exit(trusted_comments_mode(slug, n, marker, issue));
    }
    if args.get(1).map(String::as_str) == Some("--gc-clones") {
        let rest = &args[2..];
        let dry_run = rest.iter().any(|a| a == "--dry-run");
        let mut max_age_days: u64 = 30;
        let mut positional: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--dry-run" => {}
                "--max-age-days" => {
                    i += 1;
                    if let Some(v) = rest.get(i).and_then(|s| s.parse::<u64>().ok()) {
                        max_age_days = v;
                    }
                }
                other => positional.push(other),
            }
            i += 1;
        }
        let Some(&work_dir) = positional.first() else {
            eprintln!(
                "usage: pr-review-report --gc-clones <work-dir> [--dry-run] [--max-age-days N]"
            );
            std::process::exit(2);
        };
        std::process::exit(gc_clones_mode(work_dir, max_age_days, dry_run));
    }
    if args.get(1).map(String::as_str) == Some("--run-metrics") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: pr-review-report --run-metrics <trace.jsonl>");
            std::process::exit(2);
        };
        std::process::exit(run_metrics_mode(path));
    }
    if args.get(1).map(String::as_str) == Some("--record-verdict") {
        let dry_run = args.iter().any(|a| a == "--dry-run");
        // Extract `--cost <n>` / `--basis <s>` (flags with a value); everything else is positional.
        let mut cost: Option<i64> = None;
        let mut basis = String::new();
        let mut positional: Vec<&str> = Vec::new();
        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--dry-run" => {}
                "--cost" => {
                    i += 1;
                    cost = rest.get(i).and_then(|s| s.parse::<i64>().ok());
                }
                "--basis" => {
                    i += 1;
                    basis = rest.get(i).cloned().unwrap_or_default();
                }
                other => positional.push(other),
            }
            i += 1;
        }
        let (Some(&slug), Some(&pr), Some(&verdict)) =
            (positional.first(), positional.get(1), positional.get(2))
        else {
            eprintln!("usage: pr-review-report --record-verdict <owner/repo> <pr> <ready|reject|design|close|relink> [note...] [--cost <n>] [--basis <s>] [--dry-run]");
            std::process::exit(2);
        };
        let note = if positional.len() > 3 {
            positional[3..].join(" ")
        } else {
            String::new()
        };
        std::process::exit(record_verdict_mode(
            slug, pr, verdict, &note, cost, &basis, dry_run,
        ));
    }
    // No subcommand matched. GitHub is the source of truth — there is no local ledger to report; use
    // `--queue` for the review queue (the legacy no-arg report read the now-removed ledger).
    eprintln!(
        "pr-review-report — subcommands: --queue [N] | --record-verdict <owner/repo> <pr> <verdict> [note] [--cost N] [--basis s] | --trusted-comments <owner/repo> <n> [--marker m] [--issue] | --commit-closes <owner/repo> <n> | --gc-clones <work-dir> [--dry-run] | --run-metrics <trace.jsonl>"
    );
    std::process::exit(2);
}
#[cfg(test)]
mod queue_tests {
    use super::*;
    use serde_json::json;

    // --- presentable_state: the core presentability decision -----------------------------------

    // A single failing check disqualifies regardless of mergeability.
    #[test]
    fn red_ci_is_not_presentable() {
        assert_eq!(
            presentable_state(Ci::Red, Merge::Mergeable, None),
            PresentState::Red
        );
    }

    // Pending CI is not yet judgeable — never presentable, even when mergeable.
    #[test]
    fn pending_ci_is_not_presentable() {
        assert_eq!(
            presentable_state(Ci::Pending, Merge::Mergeable, None),
            PresentState::Pending
        );
    }

    // Green but conflicting is the producer's step-3d work, not presentable.
    #[test]
    fn green_conflicting_is_conflicting() {
        assert_eq!(
            presentable_state(Ci::Green, Merge::Conflicting, None),
            PresentState::Conflicting
        );
    }

    // Green + mergeable + not-yet-approved is the presentable case.
    #[test]
    fn green_mergeable_is_presentable() {
        assert_eq!(
            presentable_state(Ci::Green, Merge::Mergeable, None),
            PresentState::Presentable
        );
    }

    // A PR with no configured checks + mergeable is presentable (nothing failing/pending).
    #[test]
    fn nochecks_mergeable_is_presentable() {
        assert_eq!(
            presentable_state(Ci::NoChecks, Merge::Mergeable, None),
            PresentState::Presentable
        );
    }

    // Unknown mergeability is UNCONFIRMED (GitHub hasn't computed the merge) — not fully clean, so
    // NOT presentable; the human sees only confirmed-mergeable PRs. Green CI does not rescue it.
    #[test]
    fn green_unknown_mergeability_is_not_presentable() {
        assert_eq!(
            presentable_state(Ci::Green, Merge::Unknown, None),
            PresentState::MergeUnknown
        );
    }

    // Already human-APPROVED leaves the pending-review queue (short-circuits even a red PR).
    #[test]
    fn approved_leaves_the_queue() {
        assert_eq!(
            presentable_state(Ci::Green, Merge::Mergeable, Some("APPROVED")),
            PresentState::Approved
        );
        assert_eq!(
            presentable_state(Ci::Red, Merge::Mergeable, Some("APPROVED")),
            PresentState::Approved,
            "APPROVED short-circuits before CI"
        );
    }

    // Only the exact string "APPROVED" leaves the queue — REVIEW_REQUIRED etc. stay presentable.
    #[test]
    fn only_exact_approved_leaves_queue() {
        assert_eq!(
            presentable_state(Ci::Green, Merge::Mergeable, Some("REVIEW_REQUIRED")),
            PresentState::Presentable
        );
        assert_eq!(
            presentable_state(Ci::Green, Merge::Mergeable, Some("CHANGES_REQUESTED")),
            PresentState::Presentable
        );
    }

    // --- has_human_override: a human:* label beats an ai:ready label ----------------------------

    #[test]
    fn human_override_labels_detected() {
        for l in ["human:reject", "human:design", "human:close-candidate"] {
            let p = json!({"labels": [{"name": "ai:ready"}, {"name": l}]});
            assert!(has_human_override(&p), "must override on {l}");
        }
    }

    #[test]
    fn plain_ai_ready_is_not_overridden() {
        let p = json!({"labels": [{"name": "ai:ready"}]});
        assert!(!has_human_override(&p));
        let none = json!({"number": 1});
        assert!(!has_human_override(&none), "no labels field => no override");
    }
    // --- pr_slug: owner/repo only from real PR URLs ---------------------------------------------

    #[test]
    fn pr_slug_parses_owner_repo_only_from_real_pr_urls() {
        assert_eq!(
            pr_slug("https://github.com/cyclofinance/cyclo.site/pull/401").as_deref(),
            Some("cyclofinance/cyclo.site")
        );
        assert_eq!(
            pr_slug("https://github.com/rainlanguage/rainix/pull/1").as_deref(),
            Some("rainlanguage/rainix")
        );
        assert_eq!(pr_slug("https://example.com/o/r/pull/1"), None);
        assert_eq!(pr_slug("https://github.com/o/r/issues/1"), None);
        assert_eq!(pr_slug(""), None);
    }

    // --- render_queue: header breakdown + rows + cap --------------------------------------------

    fn qc(raw: usize, conflict: usize, red: usize, pending: usize, approved: usize) -> QueueCounts {
        QueueCounts {
            raw,
            excluded: 0,
            conflict,
            red,
            pending,
            merge_unknown: 0,
            approved,
            unconfirmed: 0,
            fetch_error: 0,
        }
    }

    // Header pins the true ai:ready -> presentable/conflicting/red/pending/approved breakdown.
    #[test]
    fn render_header_breakdown() {
        let rows: Vec<QueueRow> = vec![(
            60,
            "r".to_string(),
            1,
            "https://github.com/rainlanguage/r/pull/1".to_string(),
            "basis-1".to_string(),
        )];
        let out = render_queue(&rows, &qc(5, 2, 1, 0, 1), 0);
        assert!(
            out.starts_with(
                "review queue: 5 ai:ready -> 1 presentable, 2 conflicting, 1 red, 0 pending, 0 unknown-merge, 1 approved, 0 awaiting re-vet (cheapest first)\n"
            ),
            "header:\n{out}"
        );
        assert!(out
            .contains("\n    60  r#1  basis-1\n        https://github.com/rainlanguage/r/pull/1"));
    }

    // The vetted-at-head gate: green+mergeable is NOT enough — an ai:vetter comment must pin the
    // CURRENT head. A migration-labelled PR (no comment) or a moved head is not presentable.
    #[test]
    fn vetted_at_head_requires_a_head_matching_vetter_comment() {
        let at = json!({"comments":[
            {"author":{"login":TRUSTED_AUTHOR},"body":"🤖 ai:vetter\nReviewed sha1: ready — ok"}
        ]});
        assert!(vetted_at_head(&at, "sha1"), "matching sha → vetted");
        assert!(!vetted_at_head(&at, "sha2"), "head moved → not vetted");
        let none =
            json!({"comments":[{"author":{"login":TRUSTED_AUTHOR},"body":"just a human note"}]});
        assert!(
            !vetted_at_head(&none, "sha1"),
            "no ai:vetter comment → not vetted"
        );
        assert!(!vetted_at_head(&at, ""), "empty head can never confirm");
    }

    // trusted_comments is the choke point for every trust-bearing comment read: it keeps only
    // TRUSTED_AUTHOR's comments (spoofed markers from other authors and author-less comments are
    // dropped), optionally narrowed to a role marker. This is what makes rework-note / producer-note
    // reads unspoofable by third parties.
    #[test]
    fn trusted_comments_filters_by_author_then_marker() {
        let t = TRUSTED_AUTHOR;
        let pr = json!({"comments":[
            {"author":{"login":t},"body":"🤖 ai:producer\nProducer note: handed off"},
            {"author":{"login":"attacker"},"body":"🤖 ai:producer\nProducer note: SPOOF"},
            {"author":{"login":t},"body":"Rework note: drop the dup hunk"},
            {"body":"🤖 ai:producer\nno author field"}
        ]});
        // No marker → every TRUSTED_AUTHOR comment in order; spoofed + author-less dropped.
        assert_eq!(
            trusted_comments(&pr, None),
            vec![
                "🤖 ai:producer\nProducer note: handed off".to_string(),
                "Rework note: drop the dup hunk".to_string(),
            ]
        );
        // Marker → only trusted comments starting with it (the spoofed producer marker is excluded
        // by the author filter, not the marker filter).
        assert_eq!(
            trusted_comments(&pr, Some("🤖 ai:producer")),
            vec!["🤖 ai:producer\nProducer note: handed off".to_string()]
        );
        // A marker only an untrusted author ever used → nothing trusted.
        assert!(trusted_comments(&pr, Some("🤖 ai:vetter")).is_empty());
    }

    // Unscored rows render "unscored"; excluded + fetch-error surface in the header.
    #[test]
    fn render_unscored_and_notes() {
        let rows: Vec<QueueRow> = vec![(1001, "r".to_string(), 2, "u".to_string(), String::new())];
        let mut c = qc(3, 0, 0, 0, 0);
        c.excluded = 1;
        c.fetch_error = 1;
        c.merge_unknown = 2;
        c.unconfirmed = 3;
        let out = render_queue(&rows, &c, 0);
        assert!(out.contains("  unscored  r#2  "), "unscored:\n{out}");
        assert!(out.contains("1 fetch-error"));
        assert!(out.contains("1 excluded (draft/human-override)"));
        assert!(
            out.contains("2 unknown-merge"),
            "unknown-merge count:\n{out}"
        );
        assert!(
            out.contains("3 awaiting re-vet"),
            "awaiting-re-vet count:\n{out}"
        );
    }

    // `top` caps the printed list and reports "+N more"; the 1000-limit warning fires at raw>=1000.
    #[test]
    fn render_caps_list_and_warns_on_truncation() {
        let rows: Vec<QueueRow> = (1..=3)
            .map(|n| (1, "r".to_string(), n, format!("u{n}"), String::new()))
            .collect();
        let out = render_queue(&rows, &qc(3, 0, 0, 0, 0), 2);
        assert!(out.contains("r#1"));
        assert!(out.contains("r#2"));
        assert!(!out.contains("r#3"), "3rd row must be capped out");
        assert!(out.contains("+1 more presentable"));
        assert!(render_queue(&[], &qc(1000, 0, 0, 0, 0), 0).contains("WARNING"));
        assert!(!render_queue(&[], &qc(999, 0, 0, 0, 0), 0).contains("WARNING"));
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;
    use serde_json::json;
    // C1: empty / non-array rollups mean NO CHECKS, never green-by-default.
    #[test]
    fn ci_empty_rollup_is_nochecks() {
        assert!(classify_ci(&json!([])) == Ci::NoChecks);
        assert!(classify_ci(&Value::Null) == Ci::NoChecks);
    }

    // C2/C3: every failure conclusion and failed StatusContext state classifies RED.
    #[test]
    fn ci_fail_conclusions_and_states_are_red() {
        for c in [
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            assert!(
                classify_ci(&json!([{"status":"COMPLETED","conclusion":c}])) == Ci::Red,
                "conclusion {c}"
            );
        }
        for s in ["FAILURE", "ERROR"] {
            assert!(classify_ci(&json!([{"state":s}])) == Ci::Red, "state {s}");
        }
    }

    // C4/C5/C6: unfinished CheckRuns, non-terminal StatusContexts, and status-less items are PENDING.
    #[test]
    fn ci_unfinished_items_are_pending() {
        for st in ["QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED"] {
            assert!(
                classify_ci(&json!([{"status":st}])) == Ci::Pending,
                "status {st}"
            );
        }
        for s in ["PENDING", "EXPECTED"] {
            assert!(
                classify_ci(&json!([{"state":s}])) == Ci::Pending,
                "state {s}"
            );
        }
        assert!(
            classify_ci(&json!([{"name":"mystery"}])) == Ci::Pending,
            "no status/state must never be green"
        );
    }

    // C7: all-complete successes are GREEN (SUCCESS state contexts too).
    #[test]
    fn ci_all_success_is_green() {
        let r = json!([{"status":"COMPLETED","conclusion":"SUCCESS"},{"state":"SUCCESS"}]);
        assert!(classify_ci(&r) == Ci::Green);
    }

    // C8: one failure outranks any number of pending items.
    #[test]
    fn ci_fail_beats_pending() {
        let r = json!([{"status":"IN_PROGRESS"},{"status":"COMPLETED","conclusion":"FAILURE"}]);
        assert!(classify_ci(&r) == Ci::Red);
    }
}

#[cfg(test)]
mod commit_closes_tests {
    use super::closing_keywords;

    #[test]
    fn basic_keywords_and_separators() {
        assert_eq!(closing_keywords("Closes #99"), vec![99]);
        assert_eq!(closing_keywords("fixes #12"), vec![12]);
        assert_eq!(closing_keywords("Resolved #7"), vec![7]);
        assert_eq!(closing_keywords("closes: #5"), vec![5]);
        assert_eq!(closing_keywords("close#3"), vec![3]);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            closing_keywords("CLOSES #1 Fixes #2 rEsOlVeS #3"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn multiple_and_dedup_first_seen_order() {
        assert_eq!(
            closing_keywords("Closes #10\nCloses #2\nfixes #10"),
            vec![10, 2]
        );
    }

    #[test]
    fn bare_hash_without_keyword_is_ignored() {
        // the #217 lesson: a bare reference is not a closing keyword
        assert_eq!(closing_keywords("see #42 and refs #7"), Vec::<u64>::new());
        assert_eq!(closing_keywords("part of #100"), Vec::<u64>::new());
    }

    #[test]
    fn keyword_must_be_adjacent_to_hash() {
        // keyword and #N separated by real words do NOT link
        assert_eq!(
            closing_keywords("closes the door, see #5"),
            Vec::<u64>::new()
        );
        assert_eq!(
            closing_keywords("fixes several things in #9"),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn word_boundary_prevents_false_keywords() {
        // "closest" / "prefix" must not trigger close/fix
        assert_eq!(
            closing_keywords("the closest #5 station"),
            Vec::<u64>::new()
        );
        assert_eq!(closing_keywords("prefixes #5"), Vec::<u64>::new());
        // but a keyword at a real boundary still fires
        assert_eq!(closing_keywords("(closes #5)"), vec![5]);
    }

    #[test]
    fn no_number_after_hash() {
        assert_eq!(closing_keywords("closes #"), Vec::<u64>::new());
        assert_eq!(closing_keywords("closes #abc"), Vec::<u64>::new());
    }

    #[test]
    fn realistic_217_incident_shape() {
        // the exact shape that auto-closed #102/#86: body says Refs but a commit says Closes
        let commit = "docs(natspec): unused params + untrusted vault\n\nCloses #99 Closes #102";
        assert_eq!(closing_keywords(commit), vec![99, 102]);
    }
}

#[cfg(test)]
mod run_metrics_tests {
    use super::{is_mutation_tool, run_metrics};
    use serde_json::json;

    fn tool_line(name: &str, cmd: &str) -> String {
        json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":name,"input":{"command":cmd}}]}})
        .to_string()
    }
    fn result_line(turns: u64, dur: u64, cost: f64) -> String {
        json!({"type":"result","num_turns":turns,"duration_ms":dur,"total_cost_usd":cost,
            "usage":{"input_tokens":100,"output_tokens":200,"cache_read_input_tokens":9000,"cache_creation_input_tokens":50}}).to_string()
    }

    #[test]
    fn is_mutation_only_for_mutating_bash() {
        assert!(is_mutation_tool(
            "Bash",
            &json!({"command":"gh pr create -R x"})
        ));
        assert!(is_mutation_tool(
            "Bash",
            &json!({"command":"cd d && git commit -m x"})
        ));
        assert!(is_mutation_tool(
            "Bash",
            &json!({"command":"gh issue comment 5 --body y"})
        ));
        // read-only gh/git are NOT mutations
        assert!(!is_mutation_tool(
            "Bash",
            &json!({"command":"gh pr view 5 --json state"})
        ));
        assert!(!is_mutation_tool(
            "Bash",
            &json!({"command":"gh search prs --owner x"})
        ));
        assert!(!is_mutation_tool(
            "Bash",
            &json!({"command":"git log --oneline"})
        ));
        // non-Bash tools never count
        assert!(!is_mutation_tool(
            "Read",
            &json!({"command":"gh pr create"})
        ));
        assert!(!is_mutation_tool("Edit", &json!({})));
    }

    // A one-shot cron must never park itself: ScheduleWakeup + CronCreate are counted as wakeupCalls,
    // so any non-zero value flags a regression of the no-park rule (both are denied in settings).
    #[test]
    fn wakeup_calls_count_scheduling_tools() {
        let trace = [
            tool_line("Bash", "gh search prs --owner x"), // startup read
            tool_line("ScheduleWakeup", ""),              // PARK — must be counted
            tool_line("Bash", "gh pr create -R x"),       // first mutation at index 2
            tool_line("CronCreate", ""),                  // PARK — must be counted
            result_line(10, 1000, 1.0),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.wakeup_calls, 2, "ScheduleWakeup + CronCreate both count");
        // and they don't corrupt the tool/mutation accounting
        assert_eq!(m.tool_calls, 4);
        assert_eq!(m.first_mutation_index, Some(2));
    }

    #[test]
    fn no_wakeup_calls_in_a_clean_trace() {
        let trace = [
            tool_line("Bash", "gh pr view 5 --json state"),
            tool_line("Bash", "gh pr create -R x"),
            result_line(3, 100, 0.1),
        ]
        .join("\n");
        assert_eq!(run_metrics(&trace).wakeup_calls, 0);
    }

    #[test]
    fn startup_is_reads_before_first_mutation() {
        let trace = [
            tool_line("Bash", "gh search issues --owner x"), // recovery
            tool_line("Bash", "gh search prs --owner x"),    // recovery
            tool_line("Read", "whatever"),                   // recovery (non-mutation)
            tool_line("Bash", "gh pr create -R x"),          // FIRST MUTATION at index 3
            tool_line("Bash", "gh pr comment 1 --body y"),   // work
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.tool_calls, 5);
        assert_eq!(m.startup_tool_calls, 3);
        assert_eq!(m.first_mutation_index, Some(3));
        assert!((m.startup_pct() - 60.0).abs() < 0.01);
    }

    #[test]
    fn no_mutation_means_all_startup() {
        let trace = [
            tool_line("Bash", "gh search prs"),
            tool_line("Bash", "gh pr view 1 --json state"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.startup_tool_calls, 2);
        assert_eq!(m.first_mutation_index, None);
        assert!((m.startup_pct() - 100.0).abs() < 0.01);
    }

    #[test]
    fn first_mutation_is_the_first_only() {
        // a later read after the first mutation must NOT increment startup
        let trace = [
            tool_line("Bash", "gh search issues"),
            tool_line("Bash", "git commit -m x"), // first mutation, index 1
            tool_line("Bash", "gh pr view 2"),    // read AFTER mutation — not startup
            tool_line("Bash", "git push"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.startup_tool_calls, 1);
        assert_eq!(m.first_mutation_index, Some(1));
        assert_eq!(m.tool_calls, 4);
    }

    #[test]
    fn result_taken_from_max_turns_event() {
        // trailing short continuation results must not override the main run
        let trace = [
            tool_line("Bash", "gh pr create"),
            result_line(158, 1_600_000, 54.5), // main run
            result_line(1, 7592, 58.2),        // continuation
            result_line(1, 4272, 62.0),        // continuation
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.num_turns, 158);
        assert_eq!(m.duration_ms, 1_600_000);
        assert!((m.cost_usd - 54.5).abs() < 0.001);
        assert_eq!(m.cache_read, 9000);
    }

    #[test]
    fn malformed_lines_and_non_events_ignored() {
        let trace = [
            "not json",
            &json!({"type":"system","subtype":"init"}).to_string(),
            &tool_line("Bash", "gh pr create"),
            "{bad",
            &result_line(3, 100, 1.0),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.tool_calls, 1);
        assert_eq!(m.num_turns, 3);
    }
}

#[cfg(test)]
mod settings_tests {
    use serde_json::Value;

    // The producer AND vetter are one-shot crons that must never park themselves — ScheduleWakeup and
    // CronCreate are DENIED in both settings files so the tools are unavailable at all. This asserts
    // the deny stays in place (catches a regression where someone edits the settings and drops it).
    // Files live at the repo root, one dir up from the crate. The flake package build runs tests with
    // a filtered src that omits them, so the read is skipped there; the rs-test gate (cargo test at the
    // repo root) has the files and enforces the assertion.
    fn deny_list(rel: &str) -> Option<Vec<String>> {
        let path = format!("{}/../{}", env!("CARGO_MANIFEST_DIR"), rel);
        let text = std::fs::read_to_string(&path).ok()?;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
        Some(
            v["permissions"]["deny"]
                .as_array()
                .unwrap_or_else(|| panic!("{path}: permissions.deny is not an array"))
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        )
    }

    #[test]
    fn both_crons_deny_scheduling_tools() {
        for f in ["campaign-settings.json", "review-settings.json"] {
            let Some(deny) = deny_list(f) else {
                continue; // settings not checked out (nix build sandbox) — enforced by the rs-test gate
            };
            assert!(
                deny.iter().any(|d| d == "ScheduleWakeup"),
                "{f}: must deny ScheduleWakeup (one-shot crons must not park)"
            );
            assert!(
                deny.iter().any(|d| d == "CronCreate"),
                "{f}: must deny CronCreate (one-shot crons must not park)"
            );
        }
    }
}

#[cfg(test)]
mod record_verdict_tests {
    use super::{
        cost_from_comment, has_human_override, labels_to_remove, last_vetter_comment,
        should_skip_comment, verdict_comment, verdict_label, verdict_plan, vetted_at_head,
        VerdictPlan, TRUSTED_AUTHOR,
    };
    use serde_json::json;

    #[test]
    fn verdict_label_includes_relink() {
        assert_eq!(verdict_label("relink"), Some("ai:relink"));
    }

    // GAP-CLOSER: pins that the recording decision REFUSES when a human verdict is present. Removing
    // the guard from verdict_plan makes this fail (the leaf has_human_override test alone did not).
    #[test]
    fn verdict_plan_refuses_a_human_overridden_pr() {
        let pr = json!({"headRefOid":"abc123","labels":[{"name":"ai:ready"},{"name":"human:reject"}],"comments":[]});
        assert_eq!(
            verdict_plan(&pr, "ai:ready", "ready"),
            VerdictPlan::RefuseHuman
        );
    }

    // A native GitHub human review is sacred too — closes the TOCTOU race where a review lands between
    // the vetter's read and its record. APPROVED/CHANGES_REQUESTED refuse; a non-decision does not.
    #[test]
    fn verdict_plan_refuses_a_native_human_review() {
        for d in ["APPROVED", "CHANGES_REQUESTED"] {
            let pr = json!({"headRefOid":"abc","labels":[{"name":"ai:ready"}],"comments":[],"reviewDecision":d});
            assert_eq!(
                verdict_plan(&pr, "ai:ready", "ready"),
                VerdictPlan::RefuseHuman,
                "{d} must refuse"
            );
        }
        // REVIEW_REQUIRED (no human decision yet) records normally
        let pending = json!({"headRefOid":"abc","labels":[],"comments":[],"reviewDecision":"REVIEW_REQUIRED"});
        assert!(matches!(
            verdict_plan(&pending, "ai:ready", "ready"),
            VerdictPlan::Record { .. }
        ));
    }

    // No head sha ⇒ refuse (never post a "Reviewed :" comment).
    #[test]
    fn verdict_plan_refuses_without_a_head_sha() {
        let empty = json!({"headRefOid":"","labels":[],"comments":[]});
        assert_eq!(
            verdict_plan(&empty, "ai:ready", "ready"),
            VerdictPlan::NoSha
        );
        let missing = json!({"labels":[],"comments":[]});
        assert_eq!(
            verdict_plan(&missing, "ai:ready", "ready"),
            VerdictPlan::NoSha
        );
    }

    // Happy path: strips the other ai:*, keeps sha, no prior comment ⇒ don't skip.
    #[test]
    fn verdict_plan_records_the_label_plan() {
        let pr = json!({"headRefOid":"deadbeef","labels":[{"name":"ai:reject"},{"name":"bug"}],"comments":[]});
        match verdict_plan(&pr, "ai:ready", "ready") {
            VerdictPlan::Record {
                to_remove,
                has_target,
                sha,
                skip_comment,
            } => {
                assert_eq!(to_remove, vec!["ai:reject".to_string()]);
                assert!(!has_target);
                assert_eq!(sha, "deadbeef");
                assert!(!skip_comment);
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn verdict_label_maps_the_four_verdicts() {
        assert_eq!(verdict_label("ready"), Some("ai:ready"));
        assert_eq!(verdict_label("reject"), Some("ai:reject"));
        assert_eq!(verdict_label("design"), Some("ai:design"));
        assert_eq!(verdict_label("close"), Some("ai:close-candidate"));
        assert_eq!(verdict_label("approve"), None);
        assert_eq!(verdict_label("ai:ready"), None);
    }

    #[test]
    fn labels_to_remove_drops_other_ai_keeps_human_and_plain() {
        let current = vec![
            "ai:reject".to_string(),
            "ai:design".to_string(),
            "ai:ready".to_string(),
            "human:reject".to_string(),
            "bug".to_string(),
        ];
        let rm = labels_to_remove(&current, "ai:ready");
        // strips the OTHER ai:* verdicts...
        assert!(rm.contains(&"ai:reject".to_string()));
        assert!(rm.contains(&"ai:design".to_string()));
        // ...but never the target, a human:* label, or a plain label
        assert!(!rm.contains(&"ai:ready".to_string()), "target kept");
        assert!(!rm.contains(&"human:reject".to_string()), "human kept");
        assert!(!rm.contains(&"bug".to_string()), "non-ai kept");
        assert_eq!(rm.len(), 2);
    }

    #[test]
    fn labels_to_remove_noop_when_only_target_present() {
        let current = vec!["ai:ready".to_string(), "enhancement".to_string()];
        assert!(labels_to_remove(&current, "ai:ready").is_empty());
    }

    #[test]
    fn verdict_comment_shape_with_and_without_note() {
        assert_eq!(
            verdict_comment("abc123", "ready", "looks good", None, ""),
            "🤖 ai:vetter\nReviewed abc123: ready — looks good"
        );
        assert_eq!(
            verdict_comment("abc123", "reject", "   ", None, ""),
            "🤖 ai:vetter\nReviewed abc123: reject"
        );
        // Cost rides on its OWN line so the `Reviewed <sha>:`/`: <verdict>` matches are unaffected.
        assert_eq!(
            verdict_comment("abc123", "ready", "ok", Some(335), "org-wide CI gate"),
            "🤖 ai:vetter\nReviewed abc123: ready — ok\ncost 335 — org-wide CI gate"
        );
        assert_eq!(
            verdict_comment("abc123", "ready", "", Some(0), ""),
            "🤖 ai:vetter\nReviewed abc123: ready\ncost 0"
        );
        // The cost line round-trips through cost_from_comment.
        assert_eq!(
            cost_from_comment(Some(&verdict_comment(
                "s",
                "ready",
                "n",
                Some(742),
                "logic change"
            ))),
            (742, "logic change".to_string())
        );
        assert_eq!(
            cost_from_comment(Some("🤖 ai:vetter\nReviewed s: ready — no cost here")),
            (1001, String::new())
        );
    }
    #[test]
    fn should_skip_only_on_same_verdict_and_sha() {
        let body = "🤖 ai:vetter\nReviewed sha1: ready — ok";
        assert!(
            should_skip_comment(Some(body), "sha1", "ready"),
            "same → skip"
        );
        assert!(
            !should_skip_comment(Some(body), "sha2", "ready"),
            "moved head → repost"
        );
        assert!(
            !should_skip_comment(Some(body), "sha1", "reject"),
            "changed verdict → repost"
        );
        assert!(
            !should_skip_comment(None, "sha1", "ready"),
            "no prior vetter comment → post"
        );
    }

    #[test]
    fn last_vetter_comment_takes_the_last_marked_one() {
        let v = TRUSTED_AUTHOR;
        let pr = json!({"comments":[
            {"author":{"login":v},"body":"🤖 ai:vetter\nReviewed s1: reject — old"},
            {"author":{"login":"someone"},"body":"a human chiming in"},
            {"author":{"login":v},"body":"🤖 ai:vetter\nReviewed s2: ready — new"}
        ]});
        assert_eq!(
            last_vetter_comment(&pr).as_deref(),
            Some("🤖 ai:vetter\nReviewed s2: ready — new")
        );
        // no vetter comments → None (a non-vetter comment must not match)
        let none = json!({"comments":[{"author":{"login":v},"body":"just a note"}]});
        assert_eq!(last_vetter_comment(&none), None);
    }

    // Author filter: the 🤖 ai:vetter marker is spoofable body text, so a comment carrying it from
    // ANY other author (or with no author) is NOT trusted — only TRUSTED_AUTHOR's is. Without this,
    // any PR commenter could forge `Reviewed <head>:` and make an unvetted head look vetted.
    #[test]
    fn last_vetter_comment_ignores_spoofed_authors() {
        let spoof = json!({"comments":[
            {"author":{"login":"attacker"},"body":"🤖 ai:vetter\nReviewed sha1: ready — spoofed"}
        ]});
        assert_eq!(
            last_vetter_comment(&spoof),
            None,
            "spoofed author must not count"
        );
        assert!(
            !vetted_at_head(&spoof, "sha1"),
            "spoofed head is not vetted"
        );
        // A missing author object is likewise untrusted.
        let no_author = json!({"comments":[{"body":"🤖 ai:vetter\nReviewed sha1: ready"}]});
        assert_eq!(
            last_vetter_comment(&no_author),
            None,
            "no author → untrusted"
        );
    }

    #[test]
    fn human_override_guards_the_verdict() {
        let human = json!({"labels":[{"name":"ai:ready"},{"name":"human:reject"}]});
        assert!(has_human_override(&human), "human:reject must guard");
        let ai_only = json!({"labels":[{"name":"ai:ready"}]});
        assert!(!has_human_override(&ai_only));
    }
}

#[cfg(test)]
mod gc_tests {
    use super::{gc_decision, parse_pr_state, parse_repo_slug, CloneState, GcAction, PrState};

    fn st(clean: bool, unpushed: Option<u32>, pr: Option<PrState>, age_days: u64) -> CloneState {
        CloneState {
            clean,
            unpushed,
            pr,
            age_days,
        }
    }

    #[test]
    fn parse_repo_slug_https_ssh_and_dotted_names() {
        assert_eq!(
            parse_repo_slug("https://github.com/rainlanguage/raindex.git").as_deref(),
            Some("rainlanguage/raindex")
        );
        // ssh form + a dotted repo name; only trailing .git is stripped, inner dots preserved.
        assert_eq!(
            parse_repo_slug("git@github.com:rainlanguage/cyclo.site.git").as_deref(),
            Some("rainlanguage/cyclo.site")
        );
        // no .git suffix, trailing slash tolerated.
        assert_eq!(
            parse_repo_slug("https://github.com/cyclofinance/cyclo.site/").as_deref(),
            Some("cyclofinance/cyclo.site")
        );
        // non-github or malformed → None.
        assert_eq!(parse_repo_slug("https://example.com/x/y"), None);
        assert_eq!(parse_repo_slug("git@github.com:onlyowner"), None);
    }

    #[test]
    fn parse_pr_state_maps_states() {
        assert_eq!(parse_pr_state("OPEN"), Some(PrState::Open));
        assert_eq!(parse_pr_state("MERGED"), Some(PrState::Merged));
        assert_eq!(parse_pr_state("CLOSED"), Some(PrState::Closed));
        assert_eq!(parse_pr_state("DRAFT"), None);
    }

    // A merged or closed PR on a clean, fully-pushed clone is disposable.
    #[test]
    fn gc_deletes_merged_and_closed_when_clean() {
        assert_eq!(
            gc_decision(&st(true, Some(0), Some(PrState::Merged), 0), 30),
            GcAction::Delete("PR merged".into())
        );
        assert_eq!(
            gc_decision(&st(true, Some(0), Some(PrState::Closed), 0), 30),
            GcAction::Delete("PR closed".into())
        );
    }

    // An open PR is active work — never gc'd.
    #[test]
    fn gc_keeps_open_pr() {
        assert_eq!(
            gc_decision(&st(true, Some(0), Some(PrState::Open), 999), 30),
            GcAction::Keep("open PR".into())
        );
    }

    // Unpushed / uncommitted work is preserved even when the PR is merged — the safety guard wins
    // over the disposability rule (this is the whole reason gc is safe to run unattended).
    #[test]
    fn gc_never_deletes_dirty_or_unpushed_even_if_merged() {
        assert_eq!(
            gc_decision(&st(false, Some(0), Some(PrState::Merged), 0), 30),
            GcAction::Keep("uncommitted changes".into())
        );
        assert_eq!(
            gc_decision(&st(true, Some(3), Some(PrState::Merged), 0), 30),
            GcAction::Keep("3 unpushed commit(s)".into())
        );
        // Fail SAFE: an undeterminable unpushed count (git error / no upstream) must NEVER delete.
        // This is the exact bug the vetter caught — the old `@{u}..HEAD` + unwrap_or(0) read a
        // no-upstream error as "0 = fully pushed" and could delete the only copy of unpushed work.
        assert_eq!(
            gc_decision(&st(true, None, Some(PrState::Merged), 0), 30),
            GcAction::Keep("unpushed state unknown".into())
        );
    }

    // No resolvable PR: kept until idle past the age cap, then collected (boundary is inclusive).
    #[test]
    fn gc_age_backstop_for_no_pr_clones() {
        assert!(matches!(
            gc_decision(&st(true, Some(0), None, 13), 14),
            GcAction::Keep(_)
        ));
        assert_eq!(
            gc_decision(&st(true, Some(0), None, 14), 14),
            GcAction::Delete("no PR, idle 14d".into())
        );
    }
}
