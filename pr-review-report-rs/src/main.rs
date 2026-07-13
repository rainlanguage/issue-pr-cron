// pr-review-report — report every open PR (and logged close-candidate) that needs a HUMAN decision,
// RESPECTING reviews already done: it overlays (a) recorded review verdicts in review-verdicts.jsonl
// and (b) GitHub's own review state (APPROVED / CHANGES_REQUESTED) on top of the CI/mergeability
// signal. Rust rewrite of pr-review-report.sh, fixing the 16 bugs from the adversarial review.
//
// Usage:   pr-review-report            # all buckets
//          pr-review-report --ready    # only the reviewed-&-ready-to-merge bucket
//          pr-review-report --queue [N]                 # cheapest-first review queue
//          pr-review-report --commit-closes <owner/repo> <pr>  # fail if a commit keyword closes an out-of-index issue
//          pr-review-report --deploy <owner/repo> <pr> [--network <net>] [--dry-run]  # sanctioned Zoltu deploy of a PR branch
// Config (env overrides cron.env in CWD, then default): ORG, ORGS (org scope for --queue), PR_ASSIGNEE, CLOSE_CANDIDATES, REVIEW_VERDICTS.

use clap::{Parser, Subcommand};
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

/// Org scope for org-wide `gh search` — the SINGLE source of truth is the `ORGS` env var
/// (space- or comma-separated), exported from cron.env by the run scripts, so the queue covers
/// exactly the orgs the prompts do. Falls back to the historical default pair when unset (so a
/// bare local invocation still works). Returns flattened `--owner <org>` args, ready to splice
/// into a `gh search` arg list.
fn parse_orgs(raw: &str) -> Vec<String> {
    let orgs: Vec<String> = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let orgs = if orgs.is_empty() {
        vec!["rainlanguage".to_string(), "cyclofinance".to_string()]
    } else {
        orgs
    };
    orgs.into_iter()
        .flat_map(|o| ["--owner".to_string(), o])
        .collect()
}

fn org_owner_args() -> Vec<String> {
    parse_orgs(&std::env::var("ORGS").unwrap_or_default())
}

#[cfg(test)]
mod org_tests {
    use super::parse_orgs;

    #[test]
    fn empty_falls_back_to_default_pair() {
        let want = ["--owner", "rainlanguage", "--owner", "cyclofinance"].map(String::from);
        assert_eq!(parse_orgs(""), want);
        assert_eq!(parse_orgs("   \n"), want);
    }

    #[test]
    fn splits_on_whitespace_and_commas() {
        let want = ["--owner", "a", "--owner", "b", "--owner", "c"].map(String::from);
        assert_eq!(parse_orgs("a b c"), want);
        assert_eq!(parse_orgs("a, b,c"), want);
        assert_eq!(parse_orgs("  a\tb  c "), want);
    }

    #[test]
    fn single_org() {
        assert_eq!(
            parse_orgs("S01-Issuer"),
            ["--owner", "S01-Issuer"].map(String::from)
        );
    }
}

fn queue_mode(top: usize) {
    // Candidates come from the `ai:ready` LABEL, NOT `gh search --checks success`. That qualifier is
    // unreliable — the identical query returned 93 then 203 open PRs minutes apart, which collapsed a
    // 75-deep review queue to "1". Label search is reliable; CI/mergeability is then verified per-PR
    // below (statusCheckRollup + mergeable), never trusted from the search layer.
    // Org scope comes from ORGS (single source: cron.env), NOT a hardcoded owner list, so the
    // queue covers exactly the orgs the prompts do — change scope in one place.
    let mut search_args: Vec<String> = vec!["search".to_string(), "prs".to_string()];
    search_args.extend(org_owner_args());
    search_args.extend(
        [
            "--state",
            "open",
            "--label",
            "ai:ready",
            "--limit",
            "1000",
            "--json",
            "url,number,repository,isDraft,labels",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let search_ref: Vec<&str> = search_args.iter().map(String::as_str).collect();
    let Some(val) = gh_json(&search_ref) else {
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
        // `lower[i..]` below is a str slice that PANICS if `i` falls inside a multi-byte char (e.g.
        // an em-dash in the commit message). Keywords are ASCII, so a keyword can only start at a
        // char boundary — skip any non-boundary byte position.
        if !lower.is_char_boundary(i) {
            i += 1;
            continue;
        }
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
    // Wall-clock ms from the first timestamped trace event to the first org-mutation's result
    // (the state-recovery window). Only `user` events carry a `timestamp`, so the mutation is
    // anchored to the result of its tool call, not the assistant event that issued it. None when
    // the run never mutated, or when the anchor timestamps are absent/unparseable.
    startup_ms: Option<i64>,
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

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.fff]Z`, e.g. `2026-07-05T09:02:04.035Z`)
/// to epoch milliseconds. Self-contained (days-from-civil) so the crate keeps its zero date-lib
/// footprint; the traces are all UTC (`Z`). None on any malformed input — never panics.
fn iso_to_epoch_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    // Fixed-width fields up to the seconds; anything shorter/misshaped is rejected.
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Optional `.fff` fraction → milliseconds (pad/truncate to exactly 3 digits).
    let ms = if b.get(19) == Some(&b'.') {
        let frac: String = s[20..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        let mut f = frac.parse::<i64>().unwrap_or(0);
        for _ in frac.len()..3 {
            f *= 10;
        }
        f
    } else {
        0
    };
    // days_from_civil (Howard Hinnant): days since 1970-01-01 for a proleptic-Gregorian y-m-d.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = (if yy >= 0 { yy } else { yy - 399 }) / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + sec) * 1000 + ms)
}

/// Parse a stream-json trace: count tool calls in order, find the first mutation, and take
/// the usage/duration/cost from the result event with the most turns (the main run — trailing
/// short result events from continuations are ignored).
fn run_metrics(content: &str) -> RunMetrics {
    let mut m = RunMetrics::default();
    let mut best_turns = 0u64;
    // Wall-clock startup: anchor at the first timestamped event, close at the first mutation's
    // result. Only `user` events carry a `timestamp`, so when the first mutation tool_use is
    // seen we flag it and capture the NEXT user timestamp as the mutation's wall-clock anchor.
    let mut first_ts: Option<i64> = None;
    let mut mutation_ts: Option<i64> = None;
    let mut mutation_pending = false;
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
                                    mutation_pending = true;
                                } else {
                                    m.startup_tool_calls += 1;
                                }
                            }
                            m.tool_calls += 1;
                        }
                    }
                }
            }
            Some("user") => {
                // The only event type carrying a `timestamp`. First one seen anchors run start;
                // once a mutation is pending, the next one closes the startup window.
                if let Some(ts) = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .and_then(iso_to_epoch_ms)
                {
                    if first_ts.is_none() {
                        first_ts = Some(ts);
                    }
                    if mutation_pending {
                        mutation_ts = Some(ts);
                        mutation_pending = false;
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
    m.startup_ms = match (first_ts, mutation_ts) {
        (Some(start), Some(mut_ts)) => Some(mut_ts - start),
        _ => None,
    };
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
        "startupMs": m.startup_ms,
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
        "ai:blocked-deploy" => (
            "d93f0b",
            "AI producer: blocked on a deploy it can't complete (human)",
        ),
        "ai:blocked-infra" => (
            "e99695",
            "AI producer: blocked on an infra/tooling gap or can't classify (human)",
        ),
        "ai:blocked-on" => ("bfd4f2", "AI producer: blocked on a dependency PR"),
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

/// Pure plan for `--flag-close-candidate`: given the issue's live state, decide what to do.
#[derive(Debug, PartialEq)]
enum CloseFlagPlan {
    AlreadyClosed,
    RefuseHuman,
    Flag { add_label: bool, post_comment: bool },
}

/// A human `human:keep-open` / `human:close-candidate` ruling is sacred (refuse); a CLOSED issue is
/// moot; otherwise flag it, adding the label / posting the note only when not already present.
fn close_candidate_plan(state: &str, labels: &[String], already_noted: bool) -> CloseFlagPlan {
    if state == "CLOSED" {
        return CloseFlagPlan::AlreadyClosed;
    }
    if labels
        .iter()
        .any(|l| l == "human:keep-open" || l == "human:close-candidate")
    {
        return CloseFlagPlan::RefuseHuman;
    }
    CloseFlagPlan::Flag {
        add_label: !labels.iter().any(|l| l == "ai:close-candidate"),
        post_comment: !already_noted,
    }
}

/// `--flag-close-candidate <owner/repo> <issue> "<reason>" [--dry-run]`: the SOLE sanctioned way the
/// producer flags a closeable ISSUE — applies the `ai:close-candidate` label + a trusted
/// `🤖 ai:producer` reason comment, replacing the old local close-candidates.jsonl. GitHub state is
/// the source of truth: a closed/fixed issue drops out of the `--state open` query automatically,
/// re-flagging is idempotent, and a human `human:keep-open` / `human:close-candidate` ruling is
/// sacred (the tool refuses, exit 3). The producer NEVER closes the issue — a human does that.
fn flag_close_candidate_mode(slug: &str, issue: &str, reason: &str, dry_run: bool) -> i32 {
    if reason.trim().is_empty() {
        eprintln!(
            "usage: pr-review-report --flag-close-candidate <owner/repo> <issue> \"<reason>\" [--dry-run]"
        );
        return 2;
    }
    let Some(j) = gh_json(&[
        "issue",
        "view",
        issue,
        "-R",
        slug,
        "--json",
        "state,labels,comments",
    ]) else {
        eprintln!("error: `gh issue view {slug}#{issue}` failed — not writing on incomplete data");
        return 1;
    };
    let state = j.get("state").and_then(|s| s.as_str()).unwrap_or("");
    let labels: Vec<String> = j
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let already_noted = j
        .get("comments")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter().any(|c| {
                c.get("body")
                    .and_then(|b| b.as_str())
                    .map(|b| b.contains("Close-candidate:"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let (add_label, post_comment) = match close_candidate_plan(state, &labels, already_noted) {
        CloseFlagPlan::AlreadyClosed => {
            println!("{slug}#{issue} already closed — nothing to flag");
            return 0;
        }
        CloseFlagPlan::RefuseHuman => {
            eprintln!(
                "human decision present on {slug}#{issue} (keep-open / close-candidate); not overriding"
            );
            return 3;
        }
        CloseFlagPlan::Flag {
            add_label,
            post_comment,
        } => (add_label, post_comment),
    };
    let comment = format!("🤖 ai:producer\nClose-candidate: {reason}");

    if dry_run {
        println!("[dry-run] flag {slug}#{issue} ai:close-candidate");
        println!(
            "  label: {}",
            if add_label { "add" } else { "already present" }
        );
        println!(
            "  comment: {}",
            if post_comment {
                format!("post -> {}", comment.replace('\n', " / "))
            } else {
                "skip (already noted)".to_string()
            }
        );
        return 0;
    }

    let (color, desc) = label_meta("ai:close-candidate");
    if !gh_run(&[
        "label",
        "create",
        "ai:close-candidate",
        "-R",
        slug,
        "--color",
        color,
        "--description",
        desc,
        "--force",
    ]) {
        eprintln!("warning: could not ensure label ai:close-candidate exists in {slug}");
    }
    if add_label
        && !gh_run(&[
            "issue",
            "edit",
            issue,
            "-R",
            slug,
            "--add-label",
            "ai:close-candidate",
        ])
    {
        eprintln!("error: failed to add ai:close-candidate to {slug}#{issue}");
        return 1;
    }
    if post_comment && !gh_run(&["issue", "comment", issue, "-R", slug, "--body", &comment]) {
        eprintln!("error: labelled {slug}#{issue} but FAILED to post the reason comment");
        return 1;
    }
    println!(
        "flagged {slug}#{issue} ai:close-candidate{}",
        if post_comment {
            " [comment posted]"
        } else {
            " [comment deduped]"
        }
    );
    0
}

/// The human-facing noun for a producer state-transition comment (`<noun>: <reason>`).
fn state_noun(label: &str) -> &'static str {
    match label {
        "ai:blocked-deploy" => "Blocked-deploy",
        "ai:blocked-infra" => "Blocked-infra",
        "ai:blocked-on" => "Blocked-on",
        "ai:design" => "Design-question",
        _ => "State",
    }
}

/// The producer's human-gated state labels — the states a hand-off can land in. `ai:ready` is the
/// vetter's; the producer transitions to these via [`flag_state_mode`], never a bare prose note.
const PRODUCER_STATE_LABELS: [&str; 4] = [
    "ai:design",
    "ai:blocked-deploy",
    "ai:blocked-infra",
    "ai:blocked-on",
];

/// Pure plan for a producer state-transition ([`flag_state_mode`]). Mirrors [`verdict_plan`]'s guard —
/// a `human:*` label OR a native GitHub review is sacred (refuse) — then the label move (strip every
/// sibling `ai:*` so the PR holds exactly ONE modeled state) and whether the reason comment is a
/// dedup no-op (the identical `🤖 ai:producer` note is already posted). No head-sha requirement: a
/// producer transition is not sha-bound (unlike a vetter verdict), so a PR with no head still flags.
#[derive(Debug, PartialEq)]
enum ProducerStatePlan {
    RefuseHuman,
    Flag {
        to_remove: Vec<String>,
        has_target: bool,
        skip_comment: bool,
    },
}

fn producer_state_plan(pr_json: &Value, target: &str, comment_body: &str) -> ProducerStatePlan {
    if has_human_override(pr_json) || has_native_human_review(pr_json) {
        return ProducerStatePlan::RefuseHuman;
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
    let skip_comment = trusted_comments(pr_json, Some("🤖 ai:producer"))
        .iter()
        .any(|b| b == comment_body);
    ProducerStatePlan::Flag {
        to_remove,
        has_target,
        skip_comment,
    }
}

/// `flag-blocked-{deploy,infra,on}` / `flag-design`: the producer's OWN state-transition — move a PR
/// into exactly one modeled `ai:*` state carrying a `🤖 ai:producer` reason. This IS the FSM hand-off:
/// the producer never narrates a hand-off as a standalone prose note; it transitions here and the
/// prose rides as the reason. A human override (`human:*` label / native review) is sacred (exit 3);
/// the transition strips sibling `ai:*` labels so a PR holds one state, and re-flagging is idempotent.
fn flag_state_mode(slug: &str, pr: &str, target: &str, reason: &str, dry_run: bool) -> i32 {
    if reason.trim().is_empty() {
        eprintln!(
            "usage: pr-review-report flag-<state> <owner/repo> <pr> \"<reason>\" [--dry-run]"
        );
        return 2;
    }
    let Some(pr_json) = gh_json(&[
        "pr",
        "view",
        pr,
        "-R",
        slug,
        "--json",
        "labels,comments,reviewDecision",
    ]) else {
        eprintln!("error: `gh pr view {slug}#{pr}` failed — not writing on incomplete data");
        return 1;
    };
    let comment = format!("🤖 ai:producer\n{}: {reason}", state_noun(target));
    let (to_remove, has_target, skip_comment) =
        match producer_state_plan(&pr_json, target, &comment) {
            ProducerStatePlan::RefuseHuman => {
                eprintln!("human decision present on {slug}#{pr}; not overriding");
                return 3;
            }
            ProducerStatePlan::Flag {
                to_remove,
                has_target,
                skip_comment,
            } => (to_remove, has_target, skip_comment),
        };

    if dry_run {
        println!("[dry-run] {slug}#{pr} -> {target}");
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
            if skip_comment {
                "skip (identical note already posted)".to_string()
            } else {
                format!("post -> {}", comment.replace('\n', " / "))
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
    if !skip_comment && !gh_run(&["pr", "comment", pr, "-R", slug, "--body", &comment]) {
        eprintln!("error: labelled {slug}#{pr} {target} but FAILED to post the reason comment");
        return 1;
    }
    println!(
        "flagged {slug}#{pr} {target}{}{}",
        if to_remove.is_empty() {
            String::new()
        } else {
            format!(" (removed {})", to_remove.join(","))
        },
        if skip_comment {
            " [comment deduped]"
        } else {
            " [comment posted]"
        }
    );
    0
}

/// The first `ai:*` label a PR carries, if any (a PR should hold at most one — the FSM invariant).
fn ai_state_label(labels: &[String]) -> Option<String> {
    labels.iter().find(|l| l.starts_with("ai:")).cloned()
}

// ─────────────────────────────────────────────────────────────────────────────
// reworked-reject — the TRANSIENT-reject transition back to ready-to-vet.
//
// A human reject is not a terminal state: once a rework provably follows it, the PR re-enters the
// existing vet → queue → human lifecycle. `reworked-reject` clears `human:reject` AND every stale
// `ai:*` verdict (the code changed → it must be re-vetted from scratch), but ONLY on structural proof
// that a rework FOLLOWED the reject: the PR head commit's date must be STRICTLY NEWER than the
// `human:reject` label event. This is the one sanctioned carve-out from "never remove a `human:*`
// label" — guarded so it can never silently undo a human's still-standing reject.
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a GitHub RFC3339 UTC timestamp (`2026-07-12T10:30:00Z`) into a comparable
/// `(year, month, day, hour, min, sec)` tuple whose natural `Ord` is chronological. Tolerates a
/// trailing `Z` and fractional seconds; assumes UTC (GitHub always emits `Z`). Returns `None` if the
/// leading `YYYY-MM-DDTHH:MM:SS` shape doesn't parse — the caller then fails safe (refuses).
fn parse_rfc3339_utc(s: &str) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let (date, rest) = s.trim().split_once('T')?;
    // Drop the timezone / fractional-seconds tail; the leading HH:MM:SS is all we compare on.
    let time = rest.split(['Z', '+', '.']).next()?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let se: u32 = t.next().unwrap_or("0").parse().ok()?;
    Some((y, mo, da, h, mi, se))
}

/// The most-recent `created_at` of a `labeled` event applying `label`, from a GitHub
/// `issues/{n}/events` array (`event=="labeled"` && `label.name==<label>`). PURE (takes the parsed
/// JSON) so the label-event extraction is unit-testable. `None` when no such event exists — a reject
/// re-applied after a removal correctly wins, since the LATEST application is the one a rework must
/// post-date.
fn latest_labeled_event_date(events: Option<&Value>, label: &str) -> Option<String> {
    events?
        .as_array()?
        .iter()
        .filter(|e| {
            e.get("event").and_then(|v| v.as_str()) == Some("labeled")
                && e.pointer("/label/name").and_then(|v| v.as_str()) == Some(label)
        })
        .filter_map(|e| {
            e.get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .max_by(|a, b| match (parse_rfc3339_utc(a), parse_rfc3339_utc(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => a.cmp(b),
        })
}

/// The `reworked-reject` gate outcome.
#[derive(Debug, PartialEq)]
enum ReworkedRejectDecision {
    /// Head commit strictly newer than the reject event → clear `human:reject` + stale `ai:*`.
    Clear,
    /// Head commit not newer than the reject event → no rework followed; the human's reject stands.
    RefuseNotReworked,
    /// No `human:reject` label event found → nothing to transition (misuse / already cleared).
    RefuseNoReject,
    /// The head commit date could not be read/parsed → fail safe (never clear without proof).
    RefuseNoHeadDate,
}

/// PURE gate: may `reworked-reject` clear `human:reject`? Only when the PR head commit was made
/// STRICTLY AFTER the `human:reject` label was applied (proving a rework followed the reject). Equal
/// or older head ⇒ refuse; a missing reject event or an unparsable head date ⇒ refuse. The reject is
/// never cleared without positive proof of a later rework (fail safe: the human's decision holds).
fn reworked_reject_decision(
    head_commit_date: Option<&str>,
    reject_event_date: Option<&str>,
) -> ReworkedRejectDecision {
    let Some(reject) = reject_event_date else {
        return ReworkedRejectDecision::RefuseNoReject;
    };
    let (Some(head), Some(reject)) = (
        head_commit_date.and_then(parse_rfc3339_utc),
        parse_rfc3339_utc(reject),
    ) else {
        return ReworkedRejectDecision::RefuseNoHeadDate;
    };
    if head > reject {
        ReworkedRejectDecision::Clear
    } else {
        ReworkedRejectDecision::RefuseNotReworked
    }
}

/// `reworked-reject <owner/repo> <pr> [--dry-run]`: return a reworked `human:reject` PR to
/// ready-to-vet by REMOVING `human:reject` AND every stale `ai:*` verdict label (the code changed →
/// re-vet from scratch). GUARDED (see [`reworked_reject_decision`]): the PR head commit must strictly
/// post-date the `human:reject` label event, else it REFUSES (non-zero exit) and the reject stands.
/// The producer calls this as its FINAL step after pushing a rework commit for a `human:reject` PR
/// carrying a trusted "Rework note"; the now-unlabeled head re-enters the vetter's normal re-vet loop.
fn reworked_reject_mode(slug: &str, pr: &str, dry_run: bool) -> i32 {
    let Some(prj) = gh_json(&[
        "pr",
        "view",
        pr,
        "-R",
        slug,
        "--json",
        "headRefOid,labels,commits",
    ]) else {
        eprintln!("error: `gh pr view {slug}#{pr}` failed — not writing on incomplete data");
        return 1;
    };
    let labels: Vec<String> = prj
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !labels.iter().any(|l| l == "human:reject") {
        eprintln!(
            "error: {slug}#{pr} does not carry human:reject — nothing to transition (reworked-reject only clears an active human reject)"
        );
        return 5;
    }
    // Head commit date = the branch tip's committedDate (commits are oldest→newest, so `.last()`).
    let head_date = prj
        .get("commits")
        .and_then(|v| v.as_array())
        .and_then(|a| a.last())
        .and_then(|c| {
            c.get("committedDate")
                .or_else(|| c.get("authoredDate"))
                .and_then(|d| d.as_str())
        });
    // The `human:reject` label event, from the issue-events timeline (PRs are issues for this API).
    let events = gh_json(&[
        "api",
        "--paginate",
        &format!("repos/{slug}/issues/{pr}/events"),
    ]);
    let reject_date = latest_labeled_event_date(events.as_ref(), "human:reject");

    match reworked_reject_decision(head_date, reject_date.as_deref()) {
        ReworkedRejectDecision::RefuseNotReworked => {
            eprintln!(
                "refusing: {slug}#{pr} head commit ({}) does NOT post-date the human:reject event ({}) — no rework followed the reject; not clearing human:reject",
                head_date.unwrap_or("?"),
                reject_date.as_deref().unwrap_or("?"),
            );
            4
        }
        ReworkedRejectDecision::RefuseNoReject => {
            eprintln!(
                "refusing: no `human:reject` labeled event found on {slug}#{pr} — cannot prove a rework followed a reject"
            );
            4
        }
        ReworkedRejectDecision::RefuseNoHeadDate => {
            eprintln!(
                "error: could not read the head commit date for {slug}#{pr} — not clearing human:reject on incomplete data"
            );
            1
        }
        ReworkedRejectDecision::Clear => {
            // Remove every stale ai:* verdict FIRST, then human:reject LAST — so a mid-sequence gh
            // failure leaves the sacred human:reject in place (fail safe: the PR stays parked rather
            // than half-cleared). The PR ends carrying neither → ready-to-vet.
            let mut to_remove: Vec<String> = labels
                .iter()
                .filter(|l| l.starts_with("ai:"))
                .cloned()
                .collect();
            to_remove.push("human:reject".to_string());
            if dry_run {
                println!("[dry-run] reworked-reject {slug}#{pr} — rework post-dates the reject");
                println!(
                    "  head commit: {}  >  human:reject event: {}",
                    head_date.unwrap_or("?"),
                    reject_date.as_deref().unwrap_or("?")
                );
                println!("  labels to remove: {}", to_remove.join(", "));
                println!(
                    "  result: no human:reject, no ai:* → ready-to-vet (vetter re-vets at head)"
                );
                return 0;
            }
            let mut ok = true;
            for r in &to_remove {
                if !gh_run(&["pr", "edit", pr, "-R", slug, "--remove-label", r]) {
                    eprintln!("warning: failed to remove label {r} from {slug}#{pr}");
                    ok = false;
                }
            }
            if !ok {
                eprintln!(
                    "error: {slug}#{pr} — one or more labels failed to clear; the PR may still carry human:reject/ai:*"
                );
                return 1;
            }
            println!(
                "reworked-reject {slug}#{pr}: cleared {} → ready-to-vet (un-vetted at head)",
                to_remove.join(",")
            );
            0
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// lane bucketing — the FSM's full inventory, grouped by lane for the dashboard.
//
// `human-queue --json` emits EVERY modeled state's inventory, not just the human-action ones, so the
// dashboard can show where PRs pile up. Each producer PR lands in exactly ONE lane bucket by FSM
// precedence (a human decision dominates a stale ai:* label; a producer-blocked hand-off next; then
// an ai:ready PR splits ready↔awaiting-re-vet on head drift; then the other vetter verdicts; a
// label-less PR is a leak if the producer commented, else un-vetted).
// ─────────────────────────────────────────────────────────────────────────────

/// The four FSM lanes, plus the `Leak` anti-lane (escaped the machine — not a modeled state).
#[derive(Debug, PartialEq, Eq)]
enum Lane {
    VetLifecycle,
    VetterVerdicts,
    ProducerBlocked,
    HumanDecisions,
    Leak,
}

impl Lane {
    fn key(&self) -> &'static str {
        match self {
            Lane::VetLifecycle => "vet-lifecycle",
            Lane::VetterVerdicts => "vetter-verdicts",
            Lane::ProducerBlocked => "producer-blocked",
            Lane::HumanDecisions => "human-decisions",
            Lane::Leak => "leak",
        }
    }
}

/// The `human:*` decisions, in precedence order (a PR should carry at most one).
const HUMAN_DECISION_LABELS: [&str; 3] = ["human:reject", "human:design", "human:close-candidate"];
/// The vetter's non-`ready` verdict labels (the `ready` split is handled separately by head drift).
const VETTER_VERDICT_LABELS: [&str; 4] =
    ["ai:reject", "ai:relink", "ai:design", "ai:close-candidate"];

/// PURE: the single (lane, state) a producer PR belongs to, by FSM precedence.
/// - `ready_vetted_at_head`: for an `ai:ready` PR, `Some(false)` if the head moved past the last
///   `ai:vetter` verdict (→ `awaiting-re-vet`), else `Some(true)`/`None` keeps it in `ai:ready`.
///   (Only `ai:ready` is head-drift-split — the established `queue`/`vetted_at_head` notion — because
///   the other verdict labels can be producer-originated and carry no `ai:vetter` comment.)
/// - `producer_commented`: for a label-less PR, whether a trusted `🤖 ai:producer` comment is present
///   (a leak — the producer acted outside the FSM); a label-less PR without one is `un-vetted`.
fn classify_lane(
    labels: &[String],
    ready_vetted_at_head: Option<bool>,
    producer_commented: bool,
) -> (Lane, String) {
    let has = |name: &str| labels.iter().any(|l| l == name);
    for h in HUMAN_DECISION_LABELS {
        if has(h) {
            return (Lane::HumanDecisions, h.to_string());
        }
    }
    for b in PRODUCER_STATE_LABELS {
        if b != "ai:design" && has(b) {
            return (Lane::ProducerBlocked, b.to_string());
        }
    }
    if has("ai:ready") {
        return if ready_vetted_at_head == Some(false) {
            (Lane::VetLifecycle, "awaiting-re-vet".to_string())
        } else {
            (Lane::VetterVerdicts, "ai:ready".to_string())
        };
    }
    for v in VETTER_VERDICT_LABELS {
        if has(v) {
            return (Lane::VetterVerdicts, v.to_string());
        }
    }
    if producer_commented {
        (Lane::Leak, "leak".to_string())
    } else {
        (Lane::VetLifecycle, "un-vetted".to_string())
    }
}

/// A producer PR reduced to what lane bucketing needs — free of gh JSON so [`lanes_doc`] is
/// unit-testable without a network.
struct QueuePr {
    repo: String,
    number: u64,
    title: String,
    url: String,
    labels: Vec<String>,
    /// For an `ai:ready` PR: `Some(false)` when the head has moved past its last verdict. `None`
    /// when not computed (non-`ai:ready` PRs never need it).
    ready_vetted_at_head: Option<bool>,
    /// For a label-less PR: whether a trusted `🤖 ai:producer` comment is present (the leak signal).
    producer_commented: bool,
}

/// PURE: build the lane-grouped inventory `{ <lane>: { <state>: { count, prs:[{repo,number,url,title}] } } }`
/// from the classified PRs. Every state key appears with a stable, sorted PR list. The `Leak` lane is
/// emitted too (as `leak`), but the top-level `leaks` key stays the canonical leak view for
/// backward-compat.
fn lanes_doc(prs: &[QueuePr]) -> Value {
    // lane -> state -> Vec<pr Value>, both levels sorted (BTreeMap) for a stable snapshot diff.
    let mut lanes: std::collections::BTreeMap<
        &'static str,
        std::collections::BTreeMap<String, Vec<Value>>,
    > = std::collections::BTreeMap::new();
    for p in prs {
        let (lane, state) = classify_lane(&p.labels, p.ready_vetted_at_head, p.producer_commented);
        lanes
            .entry(lane.key())
            .or_default()
            .entry(state)
            .or_default()
            .push(serde_json::json!({
                "repo": p.repo,
                "number": p.number,
                "url": p.url,
                "title": p.title,
            }));
    }
    let doc: serde_json::Map<String, Value> = lanes
        .into_iter()
        .map(|(lane, states)| {
            let smap: serde_json::Map<String, Value> = states
                .into_iter()
                .map(|(state, items)| {
                    (
                        state,
                        serde_json::json!({ "count": items.len(), "prs": items }),
                    )
                })
                .collect();
            (lane.to_string(), Value::Object(smap))
        })
        .collect();
    Value::Object(doc)
}

/// Flat per-state counts derived from the lane doc, for a dashboard reading `counts` for tiles.
/// Lane-based (each PR counted once, human-override dominant) — distinct from the legacy label-based
/// counts (`ready`/`design`/`blocked*`) which are kept unchanged for backward-compat.
fn lane_state_count(lanes: &Value, lane: &str, state: &str) -> usize {
    lanes
        .pointer(&format!("/{lane}/{state}/count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

/// `human-queue`: the daily FSM-conformance review. Emits the FULL inventory of the machine — every
/// modeled state's PRs, grouped into four lanes (`vet-lifecycle` / `vetter-verdicts` /
/// `producer-blocked` / `human-decisions`) so the dashboard can render where PRs pile up, not just
/// the human-action states — plus the open `ai:close-candidate` issues and a loud **leak** bucket =
/// open producer PRs that carry a `🤖 ai:producer` comment but NO `ai:*`/`human:*` label (the
/// producer acting outside the FSM). The leak count is the conformance metric: it trends to zero as
/// the producer is restricted to labeled transitions. The legacy `states`/`counts`/`leaks` keys are
/// kept UNCHANGED for the dashboard's existing reads; the new `lanes` object + additive `counts` keys
/// are the full-machine view. Runtime is O(unlabeled + ai:ready producer PRs) extra `gh` calls (the
/// leak/reason check, plus the head-drift check that splits ai:ready ↔ awaiting-re-vet).
fn human_queue_mode(json_out: bool) -> i32 {
    let assignee = std::env::var("PR_ASSIGNEE").unwrap_or_else(|_| "thedavidmeister".to_string());
    // ONE search: every open producer PR with its labels — the label IS the state.
    let mut args: Vec<String> = vec!["search".into(), "prs".into()];
    args.extend(org_owner_args());
    args.extend(
        [
            "--author",
            &assignee,
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "url,number,repository,title,labels",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let argref: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(prs) = gh_json(&argref).and_then(|v| v.as_array().cloned()) else {
        eprintln!("error: `gh search prs --author {assignee}` failed — aborting rather than print a false-empty queue");
        return 1;
    };

    // One pass: the legacy label bucket (`states`, unchanged) + a per-PR `(slug,num,title,url,labels)`
    // record the lane classifier consumes. `unlabeled` = PRs with no `ai:*` label (leak candidates).
    let mut buckets: std::collections::BTreeMap<String, Vec<(String, u64, String)>> =
        std::collections::BTreeMap::new();
    let mut unlabeled: Vec<(String, u64, String)> = Vec::new();
    let mut records: Vec<(String, u64, String, String, Vec<String>)> = Vec::new();
    for p in &prs {
        let url = p
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        let Some(slug) = pr_slug(&url) else { continue };
        let num = p.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let title = p
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let labels: Vec<String> = p
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        match ai_state_label(&labels) {
            Some(state) => {
                buckets
                    .entry(state)
                    .or_default()
                    .push((slug.clone(), num, title.clone()))
            }
            None => unlabeled.push((slug.clone(), num, title.clone())),
        }
        records.push((slug, num, title, url, labels));
    }

    // Leak detection: an unlabeled PR the producer has commented on = a hand-off with no modeled
    // state (the FSM leaking). An unlabeled PR with NO producer comment is just freshly-open/unvetted.
    let mut leaks: Vec<(String, u64, String, String)> = Vec::new();
    for (slug, num, title) in &unlabeled {
        let Some(j) = gh_json(&[
            "pr",
            "view",
            &num.to_string(),
            "-R",
            slug,
            "--json",
            "comments",
        ]) else {
            continue;
        };
        let notes = trusted_comments(&j, Some("🤖 ai:producer"));
        if let Some(last) = notes.last() {
            let reason = last.replace('\n', " ");
            leaks.push((slug.clone(), *num, title.clone(), reason));
        }
    }

    // Head-drift split for ai:ready PRs: an ai:ready PR whose head moved past its last ai:vetter
    // verdict is awaiting-re-vet, not ready (the established `queue`/`vetted_at_head` notion). Fetch
    // only the ai:ready PRs that would actually reach the ai:ready lane branch (no dominating
    // human:* / ai:blocked-* label) — one `gh pr view` each.
    let leak_keys: std::collections::HashSet<(String, u64)> =
        leaks.iter().map(|(s, n, _, _)| (s.clone(), *n)).collect();
    let dominated = |labels: &[String]| {
        let has = |name: &str| labels.iter().any(|l| l == name);
        HUMAN_DECISION_LABELS.iter().any(|h| has(h))
            || PRODUCER_STATE_LABELS
                .iter()
                .any(|b| *b != "ai:design" && has(b))
    };
    let mut ready_vetted: std::collections::HashMap<(String, u64), bool> =
        std::collections::HashMap::new();
    for (slug, num, _t, _u, labels) in &records {
        if labels.iter().any(|l| l == "ai:ready") && !dominated(labels) {
            if let Some(j) = gh_json(&[
                "pr",
                "view",
                &num.to_string(),
                "-R",
                slug,
                "--json",
                "headRefOid,comments",
            ]) {
                let head = j.get("headRefOid").and_then(|v| v.as_str()).unwrap_or("");
                ready_vetted.insert((slug.clone(), *num), vetted_at_head(&j, head));
            }
        }
    }

    // The full lane-grouped inventory (each PR bucketed once, by FSM precedence).
    let queue_prs: Vec<QueuePr> = records
        .iter()
        .map(|(slug, num, title, url, labels)| QueuePr {
            repo: slug.clone(),
            number: *num,
            title: title.clone(),
            url: url.clone(),
            labels: labels.clone(),
            ready_vetted_at_head: ready_vetted.get(&(slug.clone(), *num)).copied(),
            producer_commented: leak_keys.contains(&(slug.clone(), *num)),
        })
        .collect();
    let lanes = lanes_doc(&queue_prs);

    // The open close-candidate ISSUES (close-candidate is an issue-level flag).
    let mut iargs: Vec<String> = vec!["search".into(), "issues".into()];
    iargs.extend(org_owner_args());
    iargs.extend(
        [
            "--state",
            "open",
            "--label",
            "ai:close-candidate",
            "--limit",
            "1000",
            "--json",
            "url,number,repository,title",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let iref: Vec<&str> = iargs.iter().map(String::as_str).collect();
    let close_issues: Vec<(String, u64, String)> = gh_json(&iref)
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| {
            let url = i.get("url").and_then(|u| u.as_str())?.to_string();
            let num = i.get("number").and_then(|n| n.as_u64())?;
            let title = i
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let slug = url
                .strip_prefix("https://github.com/")?
                .split("/issues/")
                .next()?
                .to_string();
            Some((slug, num, title))
        })
        .collect();

    if json_out {
        let bmap: serde_json::Map<String, Value> = buckets
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    Value::Array(
                        v.iter()
                            .map(
                                |(s, n, t)| serde_json::json!({"repo": s, "number": n, "title": t}),
                            )
                            .collect(),
                    ),
                )
            })
            .collect();
        let doc = serde_json::json!({
            "states": bmap,
            "lanes": lanes,
            "closeCandidateIssues": close_issues.iter().map(|(s,n,t)| serde_json::json!({"repo": s, "number": n, "title": t})).collect::<Vec<_>>(),
            "leaks": leaks.iter().map(|(s,n,t,r)| serde_json::json!({"repo": s, "number": n, "title": t, "reason": r})).collect::<Vec<_>>(),
            "counts": {
                // Legacy label-based counts (UNCHANGED — the dashboard reads these).
                "ready": buckets.get("ai:ready").map(|v| v.len()).unwrap_or(0),
                "design": buckets.get("ai:design").map(|v| v.len()).unwrap_or(0),
                "blockedDeploy": buckets.get("ai:blocked-deploy").map(|v| v.len()).unwrap_or(0),
                "blockedInfra": buckets.get("ai:blocked-infra").map(|v| v.len()).unwrap_or(0),
                "blockedOn": buckets.get("ai:blocked-on").map(|v| v.len()).unwrap_or(0),
                "closeCandidateIssues": close_issues.len(),
                "leaks": leaks.len(),
                "totalProducerPrs": prs.len(),
                // Additive lane-based counts (each PR counted once, human-override dominant) — the
                // states previously invisible to the dashboard.
                "unvetted": lane_state_count(&lanes, "vet-lifecycle", "un-vetted"),
                "awaitingReVet": lane_state_count(&lanes, "vet-lifecycle", "awaiting-re-vet"),
                "reject": lane_state_count(&lanes, "vetter-verdicts", "ai:reject"),
                "relink": lane_state_count(&lanes, "vetter-verdicts", "ai:relink"),
                "closeCandidatePrs": lane_state_count(&lanes, "vetter-verdicts", "ai:close-candidate"),
                "humanReject": lane_state_count(&lanes, "human-decisions", "human:reject"),
                "humanDesign": lane_state_count(&lanes, "human-decisions", "human:design"),
                "humanCloseCandidate": lane_state_count(&lanes, "human-decisions", "human:close-candidate"),
            }
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
        return 0;
    }

    // Human-readable daily review. Truncate on CHAR boundaries — titles/reasons carry unicode
    // (em-dash, middle-dot, emoji), so a byte-index slice would panic mid-codepoint.
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let show = |title: &str, items: &[(String, u64, String)]| {
        println!("\n▓▓ {title}  ({})", items.len());
        for (slug, num, t) in items {
            println!("   https://github.com/{slug}/pull/{num}");
            println!("      {}", clip(t, 66));
        }
    };
    // Print a lane/state bucket straight from the lane doc (the states without a legacy label bucket).
    let show_lane = |title: &str, lane: &str, state: &str| {
        let empty = Vec::new();
        let items = lanes
            .pointer(&format!("/{lane}/{state}/prs"))
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        println!("\n▓▓ {title}  ({})", items.len());
        for it in items {
            let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let t = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
            println!("   {url}");
            println!("      {}", clip(t, 66));
        }
    };
    println!(
        "=== HUMAN QUEUE — daily FSM-conformance review ({} open producer PRs) ===",
        prs.len()
    );
    // vet-lifecycle
    show_lane(
        "UN-VETTED — awaiting first vet",
        "vet-lifecycle",
        "un-vetted",
    );
    show_lane(
        "AWAITING-RE-VET — ai:ready head moved, re-vet needed",
        "vet-lifecycle",
        "awaiting-re-vet",
    );
    // vetter-verdicts
    if let Some(v) = buckets.get("ai:ready") {
        show("MERGE — ai:ready", v);
    }
    show_lane(
        "REWORK — ai:reject (producer reworks)",
        "vetter-verdicts",
        "ai:reject",
    );
    show_lane(
        "RELINK — ai:relink (Closes→Refs)",
        "vetter-verdicts",
        "ai:relink",
    );
    if let Some(v) = buckets.get("ai:design") {
        show("RULE — ai:design", v);
    }
    show_lane(
        "CLOSE — ai:close-candidate (PRs)",
        "vetter-verdicts",
        "ai:close-candidate",
    );
    // producer-blocked
    if let Some(v) = buckets.get("ai:blocked-deploy") {
        show("BLOCKED-DEPLOY", v);
    }
    if let Some(v) = buckets.get("ai:blocked-infra") {
        show("BLOCKED-INFRA", v);
    }
    if let Some(v) = buckets.get("ai:blocked-on") {
        show("BLOCKED-ON", v);
    }
    // human-decisions
    show_lane("HUMAN-REJECT", "human-decisions", "human:reject");
    show_lane("HUMAN-DESIGN", "human-decisions", "human:design");
    show_lane(
        "HUMAN-CLOSE-CANDIDATE",
        "human-decisions",
        "human:close-candidate",
    );
    show("CLOSE — ai:close-candidate (issues)", &close_issues);
    println!(
        "\n⚠⚠ NOT IN ANY MODELED STATE (FSM leak — should trend to 0)  ({})",
        leaks.len()
    );
    for (slug, num, t, reason) in &leaks {
        println!("   https://github.com/{slug}/pull/{num}  {}", clip(t, 52));
        println!("      {}", clip(reason, 140));
    }
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
        // FULL status (untracked INCLUDED): an untracked file could be real uncommitted WIP, so gc
        // must keep a clone with ANY dirt — never ignore untracked to reclaim more. Cleanliness is
        // the PRODUCER's job (commit real work, gitignore ephemeral artifacts, keep temp files OUT of
        // the clone, then delete the clone after submit) and the VETTER's gate (reject a PR whose
        // checkout goes dirty), NOT gc's to guess. A dirty clone left here = a hygiene bug upstream.
        let clean = git_out(dir, &["status", "--porcelain"])
            .map(|s| s.is_empty())
            .unwrap_or(false);
        // Unpushed commits = on HEAD but on NO remote-tracking branch. This works WITHOUT a configured
        // upstream (unlike `@{u}..HEAD`, which errors on an upstream-less branch); a git error stays
        // `None` (not 0) so gc_decision fails safe and keeps a clone whose push-state is unknown.
        let unpushed = git_out(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
            .and_then(|s| s.parse::<u32>().ok());
        // Only pay for the `gh pr list` network round-trip once the clone is otherwise deletable: a
        // dirty or unpushed clone is KEPT regardless of its PR state, so skipping the call for it is
        // what keeps a full pass over hundreds of clones from dragging past any timeout.
        let pr = if clean && matches!(unpushed, Some(0)) {
            resolve_pr_state(dir)
        } else {
            None
        };
        let state = CloneState {
            clean,
            unpushed,
            pr,
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
        // Stream each decision immediately: on a full disk the deletes above free space AS WE GO, and
        // progress stays visible so a long run never looks hung or gets cut off mid-scan.
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    let verb = if dry_run { "would gc" } else { "gc" };
    println!(
        "{verb}: {deleted} deleted, {kept} kept ({} clones)",
        dirs.len()
    );
    0
}

/// Args for `nix-collect-garbage`: `-d` (delete old generations + collect garbage), plus `--dry-run`
/// when previewing. Split out so it is unit-testable without spawning nix.
fn nix_gc_args(dry_run: bool) -> Vec<String> {
    let mut a = vec!["-d".to_string()];
    if dry_run {
        a.push("--dry-run".to_string());
    }
    a
}

/// Garbage-collect the nix store via `nix-collect-garbage -d` (streams nix's own output). Only
/// invoked under disk pressure (see `gc_mode` / `should_nix_gc`): a `-d` sweep evicts the warm
/// rainix/chromium build cache, so we pay that cost only when the disk actually needs the space.
/// The `result/*` symlinks stay as GC roots, so built binaries survive. Returns nonzero on failure.
fn nix_gc(dry_run: bool) -> i32 {
    println!(
        "== nix store gc ({}) ==",
        if dry_run {
            "dry-run"
        } else {
            "delete-old + collect"
        }
    );
    match Command::new("nix-collect-garbage")
        .args(nix_gc_args(dry_run))
        .status()
    {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("nix-collect-garbage exited with {:?}", s.code());
            1
        }
        Err(e) => {
            eprintln!("nix-collect-garbage failed to spawn ({e}); is nix on PATH?");
            1
        }
    }
}

/// Disk-usage percentage (the `Use%`/`Capacity` column) of the filesystem holding `path`, via
/// `df -P <path>`. `None` on any failure (spawn error, non-zero exit, unparseable output). Parsing
/// keys off the single token ending in `%`, so it survives spaces in the device/mount name.
fn disk_usage_pct(path: &str) -> Option<u8> {
    let out = Command::new("df").arg("-P").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Skip the header row; the data row carries the `NN%` capacity token.
    let data = text.lines().nth(1)?;
    let pct = data.split_whitespace().find(|t| t.ends_with('%'))?;
    pct.trim_end_matches('%').parse().ok()
}

/// Whether the nix store should be garbage-collected. Yes when disk usage is at or above the
/// threshold; and yes when usage can't be determined (`None`) — under uncertainty, guarding against
/// a full disk beats keeping the build cache warm.
fn should_nix_gc(usage: Option<u8>, threshold: u8) -> bool {
    match usage {
        Some(u) => u >= threshold,
        None => true,
    }
}

/// `--gc <work-dir> [--dry-run] [--max-age-days N] [--no-clones] [--no-nix] [--nix-threshold PCT]`:
/// unified reclaim — the per-PR/issue work clones (gc_clones_mode) AND, only under disk pressure,
/// the nix store (nix_gc). Clones run first (they free the big per-clone dirs, streaming) and always
/// run when enabled. The store is collected only when disk usage of the work-dir (or `/nix/store`)
/// is at/above `nix_threshold` percent, or usage can't be determined; otherwise the warm build cache
/// is kept. Either half can be skipped. Nonzero if either half errors.
fn gc_mode(
    work_dir: &str,
    max_age_days: u64,
    dry_run: bool,
    do_clones: bool,
    do_nix: bool,
    nix_threshold: u8,
) -> i32 {
    let mut rc = 0;
    if do_clones {
        println!("== work clones ==");
        let c = gc_clones_mode(work_dir, max_age_days, dry_run);
        if c != 0 {
            rc = c;
        }
    }
    if do_nix {
        let path = if work_dir.is_empty() {
            "/nix/store"
        } else {
            work_dir
        };
        let usage = disk_usage_pct(path);
        if should_nix_gc(usage, nix_threshold) {
            let n = nix_gc(dry_run);
            if n != 0 {
                rc = n;
            }
        } else if let Some(pct) = usage {
            // Below threshold with a known figure — skip the store sweep and keep the cache warm.
            // (usage is Some here: None routes to should_nix_gc == true above.)
            println!(
                "nix store gc SKIPPED — disk {pct}% < {nix_threshold}% threshold (cache kept warm)"
            );
        }
    }
    rc
}
// --- --deploy: the SOLE, constrained way the producer triggers a sanctioned Zoltu deploy ---------
//
// Org prod deploys are Zoltu deterministic CREATE2 (address = f(bytecode); idempotent;
// permissionless; low-stakes). The sanctioned path per repo is the repo's own
// `.github/workflows/manual-sol-artifacts.yaml` `workflow_dispatch` (which runs
// `nix develop -c rainix-sol-artifacts` / `script/Deploy.sol` under Zoltu with
// `DEPLOYMENT_KEY: secrets.PRIVATE_KEY`). This subcommand is a WRAPPER around dispatching +
// monitoring that workflow — never a reimplementation of on-chain deploy. The producer is
// banned from raw `gh workflow run`; this is the one gate it may use, so deploys are auditable
// (one tool, one behaviour) and can only happen the way we want.

/// A single `workflow_dispatch` input declaration parsed from the workflow YAML — enough to
/// construct a dispatch: its name, whether it's required, its `default`, and (for `type: choice`)
/// the allowed `options`.
#[derive(Debug, PartialEq, Clone)]
struct WorkflowInput {
    name: String,
    required: bool,
    default: Option<String>,
    options: Vec<String>,
}

/// Count of leading ASCII spaces (YAML indentation; the workflow files use spaces, never tabs).
fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

/// Strip surrounding single/double quotes and outer whitespace from a YAML scalar. (The
/// manual-sol-artifacts inputs blocks carry no inline `#` comments, so none are stripped here.)
fn strip_yaml_scalar(s: &str) -> String {
    s.trim().trim_matches(|c| c == '\'' || c == '"').to_string()
}

/// Parse the `on.workflow_dispatch.inputs` block of a workflow YAML into [`WorkflowInput`]s, in
/// declaration order. A hand-rolled, indentation-scoped scan (the crate carries only serde_json —
/// no YAML dep) covering exactly the shape the org's `manual-sol-artifacts` workflows use:
/// `inputs:` under `workflow_dispatch:`, each input a key with nested `required`/`default`/`type`/
/// `options:` (a `- item` list). Returns empty when there's no dispatch/inputs block.
fn parse_dispatch_inputs(yaml: &str) -> Vec<WorkflowInput> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    // Locate `workflow_dispatch:` and remember its indent.
    let mut wd_indent = None;
    while i < lines.len() {
        if lines[i].trim() == "workflow_dispatch:" {
            wd_indent = Some(leading_spaces(lines[i]));
            i += 1;
            break;
        }
        i += 1;
    }
    let Some(wd_indent) = wd_indent else {
        return Vec::new();
    };
    // Find `inputs:` nested under it (deeper indent); bail if we leave the block first.
    let mut inputs_indent = None;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        let ind = leading_spaces(lines[i]);
        if ind <= wd_indent {
            return Vec::new(); // left workflow_dispatch without an inputs: block
        }
        if t == "inputs:" {
            inputs_indent = Some(ind);
            i += 1;
            break;
        }
        i += 1;
    }
    let Some(inputs_indent) = inputs_indent else {
        return Vec::new();
    };
    // Parse each input entry until the block ends (a line indented back to/under `inputs:`).
    let mut out: Vec<WorkflowInput> = Vec::new();
    let mut key_indent: Option<usize> = None;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with('#') {
            i += 1;
            continue;
        }
        let ind = leading_spaces(lines[i]);
        if ind <= inputs_indent {
            break;
        }
        let ki = *key_indent.get_or_insert(ind);
        if ind == ki && t.ends_with(':') && !t.starts_with('-') {
            out.push(WorkflowInput {
                name: t.trim_end_matches(':').trim().to_string(),
                required: false,
                default: None,
                options: Vec::new(),
            });
            i += 1;
            continue;
        }
        // Property line (deeper than the key indent) of the current input.
        if let Some(cur) = out.last_mut() {
            if let Some(rest) = t.strip_prefix("default:") {
                cur.default = Some(strip_yaml_scalar(rest));
            } else if let Some(rest) = t.strip_prefix("required:") {
                cur.required = strip_yaml_scalar(rest) == "true";
            } else if t == "options:" {
                // Consume the following `- item` list (deeper than the `options:` line).
                let opt_indent = ind;
                let mut j = i + 1;
                while j < lines.len() {
                    let tt = lines[j].trim();
                    if tt.is_empty() || tt.starts_with('#') {
                        j += 1;
                        continue;
                    }
                    if leading_spaces(lines[j]) <= opt_indent {
                        break;
                    }
                    let Some(item) = tt.strip_prefix('-') else {
                        break;
                    };
                    cur.options.push(strip_yaml_scalar(item));
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Input names we treat, in priority order, as the network/chain/suite SELECTOR that `--network`
/// fills. Repos differ (`network` on rain.erc4626.words, `suite` on raindex/rain.flare), so the
/// selector is derived from the workflow, never hardcoded to one name.
const SELECTOR_NAMES: &[&str] = &["network", "net", "chain", "suite", "target"];

/// Pick which declared input `--network` fills: the first whose name matches [`SELECTOR_NAMES`]
/// (priority order), else the sole input when there's exactly one, else None (ambiguous).
fn pick_selector(inputs: &[WorkflowInput]) -> Option<usize> {
    for name in SELECTOR_NAMES {
        if let Some(idx) = inputs
            .iter()
            .position(|i| i.name.eq_ignore_ascii_case(name))
        {
            return Some(idx);
        }
    }
    if inputs.len() == 1 {
        Some(0)
    } else {
        None
    }
}

/// Resolve the selector input's value: the `--network` value if given, else the input's `default`,
/// else the sole `option` when there's exactly one (the safe auto-pick), else an error telling the
/// caller to pass `--network` (never guess among several options).
fn resolve_selector_value(inp: &WorkflowInput, network: Option<&str>) -> Result<String, String> {
    if let Some(n) = network {
        return Ok(n.to_string());
    }
    if let Some(d) = &inp.default {
        return Ok(d.clone());
    }
    match inp.options.len() {
        1 => Ok(inp.options[0].clone()),
        0 => Err(format!(
            "input `{}` needs a value — pass --network <value>",
            inp.name
        )),
        _ => Err(format!(
            "input `{}` has options {:?} and no default — pass --network <one-of-them>",
            inp.name, inp.options
        )),
    }
}

/// PURE: build the ordered `(name, value)` dispatch inputs from the workflow's declared inputs and
/// the caller's `--network`. The selector (see [`pick_selector`]) takes `--network`; any OTHER
/// required input is filled from its default/first-option; optional non-selector inputs are omitted.
/// A value constrained by `options` is validated against them. Errors (rather than dispatching a
/// wrong deploy) when it can't identify/fill the selector.
fn build_dispatch_inputs(
    inputs: &[WorkflowInput],
    network: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    if inputs.is_empty() {
        return if network.is_some() {
            Err("workflow declares no dispatch inputs, but --network was given".into())
        } else {
            Ok(Vec::new())
        };
    }
    let selector_idx = pick_selector(inputs);
    if selector_idx.is_none() && network.is_some() {
        let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
        return Err(format!(
            "cannot tell which input --network fills (inputs: {names:?}); no network/suite/chain-style selector"
        ));
    }
    let mut out = Vec::new();
    for (idx, inp) in inputs.iter().enumerate() {
        let value = if Some(idx) == selector_idx {
            resolve_selector_value(inp, network)?
        } else if inp.required {
            inp.default
                .clone()
                .or_else(|| inp.options.first().cloned())
                .ok_or_else(|| {
                    format!(
                        "required input `{}` has no default/options and is not the selector",
                        inp.name
                    )
                })?
        } else {
            continue; // optional, non-selector — omit
        };
        if !inp.options.is_empty() && !inp.options.contains(&value) {
            return Err(format!(
                "value `{}` for input `{}` is not one of its options {:?}",
                value, inp.name, inp.options
            ));
        }
        out.push((inp.name.clone(), value));
    }
    Ok(out)
}

/// PURE: the exact `gh workflow run` argv for a dispatch — also precisely what `--dry-run` prints,
/// so the previewed command is the one that would run.
fn dispatch_command(
    workflow_file: &str,
    slug: &str,
    branch: &str,
    inputs: &[(String, String)],
) -> Vec<String> {
    let mut cmd = vec![
        "gh".to_string(),
        "workflow".to_string(),
        "run".to_string(),
        workflow_file.to_string(),
        "-R".to_string(),
        slug.to_string(),
        "--ref".to_string(),
        branch.to_string(),
    ];
    for (k, v) in inputs {
        cmd.push("-f".to_string());
        cmd.push(format!("{k}={v}"));
    }
    cmd
}

/// The terminal-or-not state of a workflow run, classified from its `status`/`conclusion`.
#[derive(Debug, PartialEq, Clone, Copy)]
enum RunResult {
    Success,
    Failure,
    InProgress,
}

/// PURE: classify a `gh run view --json status,conclusion` pair (values are lowercase, unlike the
/// statusCheckRollup). A run is terminal ONLY at `status == "completed"`; anything else
/// (queued/in_progress/waiting/requested/…) is InProgress. Once completed, only `success` is
/// Success — every other conclusion (failure/cancelled/timed_out/action_required/…) is Failure.
fn classify_run(status: Option<&str>, conclusion: Option<&str>) -> RunResult {
    if status != Some("completed") {
        return RunResult::InProgress;
    }
    match conclusion {
        Some("success") => RunResult::Success,
        _ => RunResult::Failure,
    }
}

/// Human-readable one-line summary of the declared dispatch inputs, for `--dry-run` display.
fn fmt_decl(decl: &[WorkflowInput]) -> String {
    if decl.is_empty() {
        return "(none)".to_string();
    }
    decl.iter()
        .map(|i| {
            let mut s = i.name.clone();
            if i.required {
                s.push('*');
            }
            if !i.options.is_empty() {
                s.push_str(&format!(" [{}]", i.options.join("|")));
            }
            if let Some(d) = &i.default {
                s.push_str(&format!(" =default:{d}"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Run gh and return raw stdout as text; None on non-zero exit / spawn failure. The text sibling of
/// [`gh_json`], used to read a raw file via the contents API and to tail a run log.
fn gh_text(args: &[&str]) -> Option<String> {
    let out = Command::new("gh").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Read the repo's `manual-sol-artifacts` workflow AT `git_ref`, trying the `.yaml` then `.yml`
/// spelling. Returns (filename, raw content) via the GitHub contents API (raw media type), so the
/// dispatch filename + inputs are derived from the exact ref being deployed.
fn read_workflow(slug: &str, git_ref: &str) -> Option<(String, String)> {
    for file in ["manual-sol-artifacts.yaml", "manual-sol-artifacts.yml"] {
        let path = format!("repos/{slug}/contents/.github/workflows/{file}?ref={git_ref}");
        if let Some(text) = gh_text(&["api", &path, "-H", "Accept: application/vnd.github.raw"]) {
            return Some((file.to_string(), text));
        }
    }
    None
}

/// Newest `workflow_dispatch` run id for (workflow, branch), or None. `gh run list` returns
/// newest-first; the `event` field is filtered in code (no dependence on a `--event` flag).
fn latest_run_id(slug: &str, wf_file: &str, branch: &str) -> Option<u64> {
    let j = gh_json(&[
        "run",
        "list",
        "-R",
        slug,
        "--workflow",
        wf_file,
        "--branch",
        branch,
        "-L",
        "5",
        "--json",
        "databaseId,event",
    ])?;
    j.as_array()?
        .iter()
        .filter(|r| r.get("event").and_then(|e| e.as_str()) == Some("workflow_dispatch"))
        .filter_map(|r| r.get("databaseId").and_then(|d| d.as_u64()))
        .next()
}

/// After dispatching, wait for the NEW run to register: poll the newest run id until it differs
/// from the pre-dispatch snapshot `before`. Bounded (~2 min) so a lost dispatch doesn't hang.
fn await_new_run(slug: &str, wf_file: &str, branch: &str, before: Option<u64>) -> Option<u64> {
    for _ in 0..24 {
        std::thread::sleep(std::time::Duration::from_secs(5));
        if let Some(id) = latest_run_id(slug, wf_file, branch) {
            if Some(id) != before {
                return Some(id);
            }
        }
    }
    None
}

/// Poll a run to completion, streaming a short status line each tick. Bounded (~1h) so an
/// indefinitely-stuck run resolves to InProgress rather than hanging forever.
fn poll_run(slug: &str, run_id: u64) -> RunResult {
    let id = run_id.to_string();
    for _ in 0..240 {
        match gh_json(&[
            "run",
            "view",
            &id,
            "-R",
            slug,
            "--json",
            "status,conclusion",
        ]) {
            Some(j) => {
                let status = j.get("status").and_then(|v| v.as_str());
                let conclusion = j.get("conclusion").and_then(|v| v.as_str());
                match classify_run(status, conclusion) {
                    RunResult::InProgress => {
                        println!("  … {} (run {run_id})", status.unwrap_or("pending"));
                        std::thread::sleep(std::time::Duration::from_secs(15));
                    }
                    other => return other,
                }
            }
            None => {
                // Transient view error — wait and retry within the same bound.
                std::thread::sleep(std::time::Duration::from_secs(15));
            }
        }
    }
    RunResult::InProgress
}

/// The last `n` lines of the failed step's log, for post-mortem on a failed deploy.
fn failing_log_tail(slug: &str, run_id: u64, n: usize) -> Option<String> {
    let id = run_id.to_string();
    let text = gh_text(&["run", "view", &id, "-R", slug, "--log-failed"])?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(n);
    Some(all[start..].join("\n"))
}

/// `--deploy <owner/repo> <pr> [--network <net>] [--dry-run]`: trigger the repo's sanctioned
/// `manual-sol-artifacts` deploy FROM THE PR BRANCH (deploy-before-merge) and monitor it to
/// completion. SINGLE attempt per invocation — on failure it surfaces the failing log tail and
/// exits nonzero WITHOUT retrying (the "no fire-and-forget" rule: diagnose a failed deploy, never
/// blind-retry). Zoltu CREATE2 is deterministic/idempotent, so a redeploy of identical bytecode is
/// a safe no-op — no guard fights that. `--dry-run` prints the exact command and exits 0 without
/// dispatching.
fn deploy_mode(slug: &str, pr: &str, network: Option<&str>, dry_run: bool) -> i32 {
    // 1. Resolve the PR head ref/branch — deploy is FROM THE BRANCH.
    let Some(prj) = gh_json(&[
        "pr",
        "view",
        pr,
        "-R",
        slug,
        "--json",
        "headRefName,headRefOid",
    ]) else {
        eprintln!(
            "error: `gh pr view {slug}#{pr}` failed — cannot resolve the branch to deploy from"
        );
        return 1;
    };
    let branch = prj
        .get("headRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let head = prj.get("headRefOid").and_then(|v| v.as_str()).unwrap_or("");
    if branch.is_empty() {
        eprintln!("error: {slug}#{pr} has no head branch (headRefName) — cannot deploy");
        return 1;
    }
    // 2. Read the workflow at that ref and DERIVE its dispatch inputs (never hardcode input names).
    let Some((wf_file, wf_content)) = read_workflow(slug, branch) else {
        eprintln!(
            "error: no .github/workflows/manual-sol-artifacts.{{yaml,yml}} on {slug}@{branch} — this repo has no sanctioned deploy workflow"
        );
        return 1;
    };
    let decl = parse_dispatch_inputs(&wf_content);
    let dispatch = match build_dispatch_inputs(&decl, network) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot construct dispatch inputs for {wf_file}: {e}");
            return 2;
        }
    };
    let cmd = dispatch_command(&wf_file, slug, branch, &dispatch);
    let inputs_disp = if dispatch.is_empty() {
        "(none)".to_string()
    } else {
        dispatch
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    // 3. --dry-run: print the exact command that WOULD run, dispatch nothing.
    if dry_run {
        println!("[dry-run] deploy {slug}#{pr} @ {head} (branch {branch})");
        println!("  workflow: {wf_file}");
        println!("  declared inputs: {}", fmt_decl(&decl));
        println!("  dispatch inputs: {inputs_disp}");
        println!("  would run: {}", cmd.join(" "));
        return 0;
    }

    // 3b. Dispatch ONCE. Snapshot the newest run first so the resulting run can be identified.
    let before = latest_run_id(slug, &wf_file, branch);
    let cmd_ref: Vec<&str> = cmd.iter().skip(1).map(String::as_str).collect(); // drop leading "gh"
    println!("dispatching: {} (inputs: {inputs_disp})", cmd.join(" "));
    if !gh_run(&cmd_ref) {
        eprintln!("error: `gh workflow run` dispatch failed for {slug}#{pr}");
        return 1;
    }

    // 4. Identify the resulting run and poll it to completion.
    let Some(run_id) = await_new_run(slug, &wf_file, branch, before) else {
        eprintln!(
            "error: dispatched, but could not identify the resulting run within the wait window — check {slug}'s Actions tab"
        );
        return 1;
    };
    let run_url = format!("https://github.com/{slug}/actions/runs/{run_id}");
    println!("run: {run_url}");
    match poll_run(slug, run_id) {
        // 5. Success — Zoltu deterministic CREATE2; point at the run + the regenerated pins.
        RunResult::Success => {
            println!("deploy OK: {slug}#{pr} @ {head} via {wf_file} ({inputs_disp}) — {run_url}");
            println!(
                "Zoltu deterministic CREATE2: idempotent — a redeploy of identical bytecode is a no-op at the same address."
            );
            println!(
                "The regenerated deployment pins are the run's committed artifacts; re-run the PR's prod-pin tests to confirm they're green, then it's ready for the human's merge."
            );
            0
        }
        // 6. Failure — surface the failing log tail for diagnosis; do NOT retry.
        RunResult::Failure => {
            eprintln!("deploy FAILED: {slug}#{pr} — {run_url}");
            eprintln!("--- failing step log (tail) ---");
            match failing_log_tail(slug, run_id, 60) {
                Some(tail) => eprintln!("{tail}"),
                None => eprintln!("(could not fetch the failed-step log — open {run_url})"),
            }
            eprintln!(
                "Single attempt per invocation — NOT retrying. Diagnose the cause above before re-invoking --deploy."
            );
            1
        }
        RunResult::InProgress => {
            eprintln!(
                "deploy status unresolved (timed out waiting for the run to finish): {run_url}"
            );
            2
        }
    }
}

/// The CLI surface. Each subcommand maps to one `*_mode` function; clap owns all positional/flag
/// parsing, validation, and `--help`/usage (replacing the former hand-rolled `args.get(n)` dispatch).
#[derive(Parser)]
#[command(
    name = "pr-review-report",
    about = "issue-pr-cron pipeline tooling: review queue, verdicts, close-candidate flags, deploys, and gc."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

// Named `Cmd`, not `Command`, to avoid colliding with the `std::process::Command` imported above.
#[derive(Subcommand, Debug, PartialEq)]
enum Cmd {
    /// Print the human review queue (ai:ready PRs), cheapest-first.
    Queue {
        /// How many to print (default 20).
        n: Option<usize>,
    },
    /// Record an AI verdict as an ai:<verdict> label + a sha-bound comment.
    RecordVerdict {
        /// owner/repo
        slug: String,
        pr: String,
        /// ready | reject | design | close | relink
        verdict: String,
        /// One-line reason (trailing words are joined).
        note: Vec<String>,
        #[arg(long)]
        cost: Option<i64>,
        #[arg(long, default_value = "")]
        basis: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Flag an ISSUE as a close-candidate: ai:close-candidate label + trusted reason comment.
    FlagCloseCandidate {
        /// owner/repo
        slug: String,
        issue: String,
        /// Reason (trailing words are joined).
        reason: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the trusted account's comments on a PR (or issue, with --issue), most-recent last.
    TrustedComments {
        /// owner/repo
        slug: String,
        n: String,
        #[arg(long)]
        marker: Option<String>,
        #[arg(long)]
        issue: bool,
    },
    /// Fail if a commit-message closing keyword references an issue absent from the PR's live closingIssuesReferences.
    CommitCloses {
        /// owner/repo
        slug: String,
        pr: String,
    },
    /// Trigger the repo's sanctioned Zoltu deploy (manual-sol-artifacts.yaml) for a PR's branch.
    Deploy {
        /// owner/repo
        slug: String,
        pr: String,
        #[arg(long)]
        network: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Garbage-collect the per-PR/issue work clones directly under <work-dir>.
    GcClones {
        work_dir: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 30)]
        max_age_days: u64,
    },
    /// Unified reclaim: the work clones (gc-clones), always; the nix store (nix-collect-garbage -d)
    /// only when the disk is under pressure (usage >= --nix-threshold), so the build cache stays warm.
    Gc {
        /// Required unless --no-clones.
        work_dir: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 30)]
        max_age_days: u64,
        #[arg(long)]
        no_clones: bool,
        #[arg(long)]
        no_nix: bool,
        /// Only run the nix store gc when disk usage is at/above this percent (default 85).
        #[arg(long, default_value_t = 85)]
        nix_threshold: u8,
    },
    /// Emit one enriched per-run metrics JSON line distilled from a stream-json trace.
    RunMetrics { trace: String },
    /// The producer's whole in-flight worklist in ONE call: own open PRs with CI/failing-checks/
    /// mergeState/threads/closes/markers and a computed next_action. Replaces the hand-rolled startup.
    Worklist {
        #[arg(long)]
        json: bool,
        /// Bypass the read-through cache entirely (always fetch fresh).
        #[arg(long)]
        no_cache: bool,
    },
    /// Open issues NOT already covered by an open PR (the dedup the producer hand-rolled in `.jq`).
    UncoveredIssues {
        #[arg(long)]
        json: bool,
    },
    /// Producer transition: flag a PR into ai:blocked-deploy (a deploy the producer can't complete).
    FlagBlockedDeploy {
        /// owner/repo
        slug: String,
        pr: String,
        /// Reason (trailing words are joined).
        reason: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Producer transition: flag a PR into ai:blocked-infra (infra/tooling gap OR can't-classify).
    FlagBlockedInfra {
        /// owner/repo
        slug: String,
        pr: String,
        reason: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Producer transition: flag a PR into ai:blocked-on (waiting on a dependency PR).
    FlagBlockedOn {
        /// owner/repo
        slug: String,
        pr: String,
        reason: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Producer transition: flag a PR into ai:design (raises a design question a human must rule).
    FlagDesign {
        /// owner/repo
        slug: String,
        pr: String,
        reason: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Producer transition: a reworked human:reject PR back to ready-to-vet. Clears human:reject +
    /// every stale ai:* verdict — GUARDED on the head commit post-dating the human:reject event.
    ReworkedReject {
        /// owner/repo
        slug: String,
        pr: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// The daily FSM-conformance review: every open item grouped by human-gated state, plus a
    /// loud "NOT IN ANY MODELED STATE" leak bucket. The instrument for the daily status check.
    HumanQueue {
        #[arg(long)]
        json: bool,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// worklist + uncovered-issues — the producer's STATE-LOAD, done by the tool.
//
// Run data showed a producer spends ~half its tool calls hand-reconstructing GitHub
// state every run (cross-org `gh search`, per-PR `gh pr view` loops, throwaway `.jq`
// dedup) before doing any work. Cost scales with tool calls — each call re-reads the
// whole ~95k-token context — so that startup was ~half the run's cost and wall-clock.
// These two subcommands ARE the FSM's state-load: one call each, done in-process, so
// the producer loads its whole in-flight worklist and its candidate new-issue set
// without re-improvising enumeration in bash. This keeps state-load inside the tool,
// per the "prompts only use the rust tool for I/O" doctrine.
// ─────────────────────────────────────────────────────────────────────────────

/// The producer's next step for one of its own open PRs — the FSM state `worklist` computes so the
/// producer knows WHICH PRs need action without re-deriving it from scratch each run.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NextAction {
    GreenReady,   // green + mergeable + no open threads -> present to the human (step 2z)
    Deploy,       // red prod-pin/testProdDeploy*, or green "REQUIRES redeploy at land" (3b iv)
    Conflict3d,   // DIRTY/BEHIND -> resolve conflicts (3d)
    Coderabbit3e, // clean CI but unresolved review threads (3e)
    Screenshot3c, // UI PR missing its screenshot (3c)
    Needs3b,      // red, fixable, not parked (3b)
    ParkedSkip,   // design-flicked / handed-off -> do NOT re-touch this run
    Wait,         // CI still in flight -> nothing to do yet
}

impl NextAction {
    fn as_str(self) -> &'static str {
        match self {
            NextAction::GreenReady => "green-ready",
            NextAction::Deploy => "deploy",
            NextAction::Conflict3d => "conflict-3d",
            NextAction::Coderabbit3e => "coderabbit-3e",
            NextAction::Screenshot3c => "screenshot-3c",
            NextAction::Needs3b => "needs-3b",
            NextAction::ParkedSkip => "parked-skip",
            NextAction::Wait => "wait",
        }
    }
}

/// The derived per-PR signals the pure classifier consumes. Separated from the gh JSON so
/// `next_action` is unit-testable without a network.
struct PrSignals {
    ci: Ci,
    merge_state: String,
    unresolved_threads: usize,
    has_deploy_trigger: bool,
    deploy_done_at_head: bool,
    parked: bool,
    ui_missing_screenshot: bool,
    /// The PR's modeled `ai:*` state label, if any. When it is a human-gated state (`ai:design` /
    /// `ai:blocked-*` / `ai:close-candidate`), the label IS the state and the producer leaves the PR
    /// parked — only un-labeled PRs are classified from CI/mergeState.
    state_label: Option<String>,
}

/// PURE FSM classifier: given a PR's derived signals, what should the producer do with it this run?
/// Priority is deliberate: an outstanding deploy is the only thing that greens a prod-pin (and a green
/// "REQUIRES redeploy" PR is not truly landable), so it leads. Then red PRs (fix, or if parked skip).
/// A pending CI just waits. Clean-CI PRs route conflict > open-threads > missing-screenshot, else they
/// are green-ready for the human. A `parked` flag only suppresses re-touching a STILL-RED PR — a PR
/// that has since gone green surfaces as green-ready regardless of past parking.
fn next_action(s: &PrSignals) -> NextAction {
    // A PR the producer has already moved into a modeled human-gated state (design / blocked-* /
    // close-candidate) is PARKED for a human — the label IS the state, so the producer does not
    // re-touch it and does not re-derive a state from CI. Only un-labeled PRs fall through to the
    // CI/mergeState classifier below.
    if let Some(l) = &s.state_label {
        if PRODUCER_STATE_LABELS.contains(&l.as_str()) || l == "ai:close-candidate" {
            return NextAction::ParkedSkip;
        }
    }
    if s.has_deploy_trigger && !s.deploy_done_at_head {
        return NextAction::Deploy;
    }
    match s.ci {
        Ci::Red => {
            if s.parked {
                NextAction::ParkedSkip
            } else {
                NextAction::Needs3b
            }
        }
        Ci::Pending => NextAction::Wait,
        Ci::Green | Ci::NoChecks => {
            let m = s.merge_state.as_str();
            if m == "DIRTY" || m == "BEHIND" {
                NextAction::Conflict3d
            } else if s.unresolved_threads > 0 {
                NextAction::Coderabbit3e
            } else if s.ui_missing_screenshot {
                NextAction::Screenshot3c
            } else {
                NextAction::GreenReady
            }
        }
    }
}

/// Display names of the FAILING checks in a statusCheckRollup — so the producer knows which check to
/// fix without a second `gh pr checks` call. Same fail-set as `classify_ci`.
fn failing_check_names(rollup: &Value) -> Vec<String> {
    let empty = Vec::new();
    rollup
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|it| {
            let concl = it.get("conclusion").and_then(|v| v.as_str());
            let state = it.get("state").and_then(|v| v.as_str());
            let failing = matches!(
                concl,
                Some("FAILURE")
                    | Some("TIMED_OUT")
                    | Some("CANCELLED")
                    | Some("ACTION_REQUIRED")
                    | Some("STARTUP_FAILURE")
            ) || matches!(state, Some("FAILURE") | Some("ERROR"));
            if failing {
                it.get("name")
                    .or_else(|| it.get("context"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            } else {
                None
            }
        })
        .collect()
}

/// Open issues NOT covered by any open PR. PURE: `covered` is the set of (repo, issue#) an open PR's
/// closing keywords link, and coverage is SAME-REPO only (a `Closes #5` in repoA never covers repoB#5).
fn uncovered(
    issues: &[(String, u64)],
    covered: &std::collections::HashSet<(String, u64)>,
) -> Vec<(String, u64)> {
    issues
        .iter()
        .filter(|k| !covered.contains(*k))
        .cloned()
        .collect()
}

/// Cache freshness for a stored PR row (the tool's own read-through cache — see `worklist_mode`).
/// Serve the cached detail (skip the expensive per-PR fetch) IFF the PR is provably UNCHANGED and
/// SETTLED: same `updatedAt` (bumped by any push/comment/label — the cheap signal available from the
/// PR search), a TERMINAL ci ("green"/"red", never "pending"/"nochecks" — an in-flight PR is always
/// re-fetched), and within TTL. This can only ever SKIP a fetch for an unchanged settled PR; it never
/// serves a PR whose `updatedAt` moved. Correctness holds with the cache empty or `--no-cache`.
///
/// DELIBERATE TRADEOFF (not a bug): the freshness key is `updatedAt` + terminal-CI + TTL, NOT the
/// head OID. A CI *re-run on the SAME commit* that flips green↔red without bumping `updatedAt` can be
/// served ≤TTL-stale. This is bounded and accepted: `worklist` is a TRIAGE load (what to work next),
/// and merge-readiness is re-verified at head by the `queue` command before a human lands anything.
/// Adding head-oid would not help this case (the commit is unchanged); shrink `WORKLIST_TTL_SECS` if a
/// tighter bound is ever needed.
fn cache_fresh(
    row_updated: &str,
    row_ci: &str,
    row_fetched: i64,
    cur_updated: &str,
    now: i64,
    ttl: i64,
) -> bool {
    row_updated == cur_updated
        && (row_ci == "green" || row_ci == "red")
        && (now - row_fetched) < ttl
}

fn ci_str(ci: Ci) -> &'static str {
    match ci {
        Ci::Red => "red",
        Ci::Pending => "pending",
        Ci::NoChecks => "nochecks",
        Ci::Green => "green",
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn pr_assignee() -> String {
    std::env::var("PR_ASSIGNEE").unwrap_or_else(|_| "thedavidmeister".to_string())
}

fn worklist_cache_path() -> String {
    std::env::var("WORKLIST_CACHE")
        .unwrap_or_else(|_| "/home/gildlab/issue-pr-cron/.worklist-cache.json".to_string())
}

/// The JSON read-through cache: `{ "owner/repo#num": { updated_at, ci, fetched_at, detail } }`.
/// A plain file (not sqlite) keeps this tool dependency-free — the cron depends on every subcommand
/// building, and a ~hundreds-of-rows, single-process (flock'd), once-per-run cache needs none of
/// sqlite's concurrency/indexing. `--no-cache` bypasses it; a missing/corrupt file = empty cache.
fn load_cache() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(worklist_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_cache(map: &serde_json::Map<String, Value>) {
    if let Ok(s) = serde_json::to_string(&Value::Object(map.clone())) {
        let _ = std::fs::write(worklist_cache_path(), s);
    }
}

/// Fetch one PR's rich detail + its unresolved-review-thread count. `None` on a transient gh failure
/// (the caller drops the PR from the list rather than reporting a false state).
fn fetch_pr_detail(slug: &str, num: u64) -> Option<Value> {
    let n = num.to_string();
    let mut j = gh_json(&[
        "pr", "view", &n, "-R", slug, "--json",
        "number,title,url,mergeable,mergeStateStatus,statusCheckRollup,reviewDecision,headRefOid,commits,closingIssuesReferences,createdAt,updatedAt,comments,labels,isDraft,body,files",
    ])?;
    let (owner, repo) = slug.split_once('/')?;
    let q = format!(
        "query{{repository(owner:\"{owner}\",name:\"{repo}\"){{pullRequest(number:{num}){{reviewThreads(first:50){{nodes{{isResolved}}}}}}}}}}"
    );
    let threads = gh_json(&["api", "graphql", "-f", &format!("query={q}")])
        .and_then(|v| {
            v.pointer("/data/repository/pullRequest/reviewThreads/nodes")
                .and_then(|n| n.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|t| t.get("isResolved").and_then(|b| b.as_bool()) == Some(false))
                        .count()
                })
        })
        .unwrap_or(0);
    if let Some(obj) = j.as_object_mut() {
        obj.insert("unresolvedThreads".into(), Value::from(threads));
    }
    Some(j)
}

/// Derive a PR's signals + next_action from its detail JSON (pure given the JSON).
fn worklist_row(slug: &str, detail: &Value) -> Value {
    let rollup = detail
        .get("statusCheckRollup")
        .cloned()
        .unwrap_or(Value::Null);
    let ci = classify_ci(&rollup);
    let failing = failing_check_names(&rollup);
    let merge_state = detail
        .get("mergeStateStatus")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let threads = detail
        .get("unresolvedThreads")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let closes: Vec<u64> = detail
        .get("closingIssuesReferences")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("number").and_then(|n| n.as_u64()))
                .collect()
        })
        .unwrap_or_default();
    let head = detail
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // markers — best-effort triage signals (the producer re-confirms from the log when it acts):
    let body = detail.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let requires_redeploy = body.contains("REQUIRES redeploy at land")
        || trusted_comments(detail, None)
            .iter()
            .any(|c| c.contains("REQUIRES redeploy at land"));
    // a green PR flagged for redeploy, OR a red prod-pin check, is the deploy case
    let deploy_pin_red = ci == Ci::Red
        && failing.iter().any(|n| {
            let n = n.to_ascii_lowercase();
            n.contains("prod") && n.contains("deploy") || n.contains("testproddeploy")
        });
    let has_deploy_trigger = requires_redeploy || deploy_pin_red;
    let trusted = trusted_comments(detail, None);
    // HEAD-SCOPED: a deploy counts as done ONLY when a trusted note records a deploy SUCCESS /
    // deploy-confirmed AND names the CURRENT head SHA. A bare `deploy-confirmed` from a PRIOR head
    // must NOT count — else a PR deploy-confirmed at head A, then pushed new bytecode (head B, flagged
    // REQUIRES redeploy), would read done, skip the redeploy, and surface ready with UNDEPLOYED
    // bytecode (defeats deploy-before-merge). The producer's deploy-confirmed note embeds the head SHA
    // (campaign-prompt 3b (iv)) precisely so this head-scoped match works.
    let deploy_done_at_head = trusted.iter().any(|c| {
        (c.contains("deploy") && (c.contains("SUCCESS") || c.contains("deploy-confirmed")))
            && c.contains(head)
    });
    // parked: a design-clarification note, or a hand-off note, from the trusted producer account
    let design_flicked = trusted.iter().any(|c| {
        c.contains("design-clarification")
            || c.contains("flick to design")
            || c.contains("FLICK TO DESIGN")
    });
    let handed_off = trusted.iter().any(|c| {
        c.contains("HAND OFF")
            || c.contains("hand-off")
            || c.contains("Producer note:") && c.contains("infra")
    });
    let has_3b_attempt = detail
        .get("commits")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter().any(|c| {
                c.pointer("/messageHeadline")
                    .and_then(|m| m.as_str())
                    .map(|m| m.contains("[3b-attempt]"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let parked = design_flicked || handed_off;
    // UI PR missing a screenshot: touches a webapp/ui/site path AND no shots/<n>.png marker
    let touches_ui = detail
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter().any(|f| {
                let p = f.get("path").and_then(|p| p.as_str()).unwrap_or("");
                p.contains("packages/webapp")
                    || p.contains("packages/ui-components")
                    || (p.starts_with("site/") && p.ends_with(".html"))
            })
        })
        .unwrap_or(false);
    let num = detail.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    let has_shot = trusted.iter().any(|c| {
        c.contains(&format!("shots/{num}.png")) || c.contains("screenshot pending (manual)")
    });
    let ui_missing_screenshot = touches_ui && !has_shot;

    let labels: Vec<String> = detail
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let state_label = ai_state_label(&labels);
    let sig = PrSignals {
        ci,
        merge_state: merge_state.clone(),
        unresolved_threads: threads,
        has_deploy_trigger,
        deploy_done_at_head,
        parked,
        ui_missing_screenshot,
        state_label: state_label.clone(),
    };
    let action = next_action(&sig);

    serde_json::json!({
        "repo": slug,
        "number": num,
        "url": detail.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "title": detail.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "ci": ci_str(ci),
        "failingChecks": failing,
        "mergeState": merge_state,
        "unresolvedThreads": threads,
        "closes": closes,
        "createdAt": detail.get("createdAt").and_then(|v| v.as_str()).unwrap_or(""),
        "updatedAt": detail.get("updatedAt").and_then(|v| v.as_str()).unwrap_or(""),
        "isDraft": detail.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false),
        "markers": {
            "requiresRedeploy": requires_redeploy,
            "deployDoneAtHead": deploy_done_at_head,
            "designFlicked": design_flicked,
            "handedOff": handed_off,
            "has3bAttempt": has_3b_attempt,
            "screenshotPending": has_shot,
        },
        "stateLabel": state_label,
        "nextAction": action.as_str(),
    })
}

fn worklist_mode(json_out: bool, use_cache: bool) -> i32 {
    let assignee = pr_assignee();
    let mut search: Vec<String> = vec!["search".into(), "prs".into()];
    search.extend(org_owner_args());
    search.extend(
        [
            "--author",
            &assignee,
            "--state",
            "open",
            "--limit",
            "500",
            "--json",
            "number,repository,url,updatedAt",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let sref: Vec<&str> = search.iter().map(String::as_str).collect();
    let Some(val) = gh_json(&sref) else {
        eprintln!("error: `gh search prs --author {assignee}` failed (transient API/auth?) — aborting rather than report a falsely-empty worklist");
        return 1;
    };
    let empty = Vec::new();
    let arr = val.as_array().unwrap_or(&empty);

    let mut cache = if use_cache {
        load_cache()
    } else {
        serde_json::Map::new()
    };
    let now = now_unix();
    let ttl: i64 = std::env::var("WORKLIST_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10800); // 3h
    let mut live_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<Value> = Vec::new();

    for p in arr {
        let (Some(num), Some(repo)) = (
            p.get("number").and_then(|n| n.as_u64()),
            p.get("repository")
                .and_then(|r| r.get("nameWithOwner"))
                .and_then(|s| s.as_str()),
        ) else {
            continue;
        };
        let cur_updated = p.get("updatedAt").and_then(|u| u.as_str()).unwrap_or("");
        let key = format!("{repo}#{num}");
        live_keys.insert(key.clone());

        // cache read-through
        if use_cache {
            if let Some(row) = cache.get(&key) {
                let ru = row.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                let rci = row.get("ci").and_then(|v| v.as_str()).unwrap_or("");
                let rf = row.get("fetched_at").and_then(|v| v.as_i64()).unwrap_or(0);
                if cache_fresh(ru, rci, rf, cur_updated, now, ttl) {
                    if let Some(d) = row.get("detail") {
                        rows.push(worklist_row(repo, d));
                        continue;
                    }
                }
            }
        }
        // miss -> fetch fresh
        let Some(detail) = fetch_pr_detail(repo, num) else {
            continue;
        };
        let ci = ci_str(classify_ci(
            detail.get("statusCheckRollup").unwrap_or(&Value::Null),
        ));
        if use_cache {
            cache.insert(
                key,
                serde_json::json!({ "updated_at": cur_updated, "ci": ci, "fetched_at": now, "detail": detail }),
            );
        }
        rows.push(worklist_row(repo, &detail));
    }

    if use_cache {
        // eviction: drop merged/closed PRs (not in the live set) and any row older than 7d.
        let hard = now - 7 * 24 * 3600;
        cache.retain(|k, v| {
            live_keys.contains(k)
                && v.get("fetched_at").and_then(|f| f.as_i64()).unwrap_or(0) > hard
        });
        save_cache(&cache);
    }

    // sort: actionable first (by NextAction rank), then oldest updated first
    rows.sort_by(|a, b| {
        let ra = action_rank(a.get("nextAction").and_then(|s| s.as_str()).unwrap_or(""));
        let rb = action_rank(b.get("nextAction").and_then(|s| s.as_str()).unwrap_or(""));
        ra.cmp(&rb).then_with(|| {
            a.get("updatedAt")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .cmp(b.get("updatedAt").and_then(|s| s.as_str()).unwrap_or(""))
        })
    });

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(rows)).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!("worklist: {} open PRs by {assignee}\n", rows.len());
        for r in &rows {
            let fc = r
                .get("failingChecks")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            println!(
                "  [{:>12}] {}#{}  ci={} merge={} threads={}{}",
                r.get("nextAction").and_then(|v| v.as_str()).unwrap_or(""),
                r.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
                r.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                r.get("ci").and_then(|v| v.as_str()).unwrap_or(""),
                r.get("mergeState").and_then(|v| v.as_str()).unwrap_or(""),
                r.get("unresolvedThreads")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                if fc.is_empty() {
                    String::new()
                } else {
                    format!("  failing=[{fc}]")
                },
            );
        }
    }
    0
}

/// Rank a nextAction string for sort (mirrors NextAction::rank; kept string-keyed for the Value rows).
fn action_rank(a: &str) -> u8 {
    match a {
        "deploy" => 0,
        "needs-3b" => 1,
        "conflict-3d" => 2,
        "coderabbit-3e" => 3,
        "screenshot-3c" => 4,
        "green-ready" => 5,
        "wait" => 6,
        _ => 7, // parked-skip
    }
}

fn uncovered_issues_mode(json_out: bool) -> i32 {
    // open issues
    let mut isearch: Vec<String> = vec!["search".into(), "issues".into()];
    isearch.extend(org_owner_args());
    isearch.extend(
        [
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "number,repository,url,title,labels",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let iref: Vec<&str> = isearch.iter().map(String::as_str).collect();
    let Some(ival) = gh_json(&iref) else {
        eprintln!("error: `gh search issues` failed — aborting rather than report a falsely-empty issue set");
        return 1;
    };
    // open PRs + their closing refs
    let mut psearch: Vec<String> = vec!["search".into(), "prs".into()];
    psearch.extend(org_owner_args());
    psearch.extend(
        [
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "number,repository,title,body",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let pref: Vec<&str> = psearch.iter().map(String::as_str).collect();
    let Some(pval) = gh_json(&pref) else {
        eprintln!("error: `gh search prs` failed — aborting");
        return 1;
    };

    let mut covered: std::collections::HashSet<(String, u64)> = std::collections::HashSet::new();
    for p in pval.as_array().unwrap_or(&Vec::new()) {
        let Some(repo) = p
            .get("repository")
            .and_then(|r| r.get("nameWithOwner"))
            .and_then(|s| s.as_str())
        else {
            continue;
        };
        // Closing keywords in title+body (same repo). `gh search prs` CANNOT return
        // `closingIssuesReferences` (that field is `gh pr view`-only — requesting it makes the
        // whole search error out), so closing-keyword extraction IS the coverage signal — the same
        // signal the producer's hand-rolled `jq` dedup used.
        let text = format!(
            "{} {}",
            p.get("title").and_then(|t| t.as_str()).unwrap_or(""),
            p.get("body").and_then(|b| b.as_str()).unwrap_or("")
        );
        for n in closing_keywords(&text) {
            covered.insert((repo.to_string(), n));
        }
    }

    let mut issues: Vec<(String, u64)> = Vec::new();
    let mut meta: std::collections::HashMap<(String, u64), Value> =
        std::collections::HashMap::new();
    for it in ival.as_array().unwrap_or(&Vec::new()) {
        let Some(repo) = it
            .get("repository")
            .and_then(|r| r.get("nameWithOwner"))
            .and_then(|s| s.as_str())
        else {
            continue;
        };
        let Some(num) = it.get("number").and_then(|n| n.as_u64()) else {
            continue;
        };
        let k = (repo.to_string(), num);
        issues.push(k.clone());
        meta.insert(k, it.clone());
    }

    let open = uncovered(&issues, &covered);
    if json_out {
        let arr: Vec<Value> = open.iter().filter_map(|k| meta.get(k).cloned()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!("uncovered issues (no open PR): {}\n", open.len());
        for (repo, num) in &open {
            let title = meta
                .get(&(repo.clone(), *num))
                .and_then(|m| m.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            println!(
                "  {repo}#{num}  {}",
                &title.chars().take(70).collect::<String>()
            );
        }
    }
    0
}

fn main() {
    let code = match Cli::parse().command {
        Cmd::Queue { n } => {
            queue_mode(n.unwrap_or(20));
            0
        }
        Cmd::RecordVerdict {
            slug,
            pr,
            verdict,
            note,
            cost,
            basis,
            dry_run,
        } => record_verdict_mode(&slug, &pr, &verdict, &note.join(" "), cost, &basis, dry_run),
        Cmd::FlagCloseCandidate {
            slug,
            issue,
            reason,
            dry_run,
        } => flag_close_candidate_mode(&slug, &issue, &reason.join(" "), dry_run),
        Cmd::TrustedComments {
            slug,
            n,
            marker,
            issue,
        } => trusted_comments_mode(&slug, &n, marker.as_deref(), issue),
        Cmd::CommitCloses { slug, pr } => commit_closes_mode(&slug, &pr),
        Cmd::Deploy {
            slug,
            pr,
            network,
            dry_run,
        } => deploy_mode(&slug, &pr, network.as_deref(), dry_run),
        Cmd::GcClones {
            work_dir,
            dry_run,
            max_age_days,
        } => gc_clones_mode(&work_dir, max_age_days, dry_run),
        Cmd::Gc {
            work_dir,
            dry_run,
            max_age_days,
            no_clones,
            no_nix,
            nix_threshold,
        } => {
            let do_clones = !no_clones;
            let do_nix = !no_nix;
            let wd = work_dir.unwrap_or_default();
            if do_clones && wd.is_empty() {
                eprintln!("error: gc needs <work-dir> unless --no-clones is given");
                std::process::exit(2);
            }
            gc_mode(&wd, max_age_days, dry_run, do_clones, do_nix, nix_threshold)
        }
        Cmd::RunMetrics { trace } => run_metrics_mode(&trace),
        Cmd::Worklist { json, no_cache } => worklist_mode(json, !no_cache),
        Cmd::UncoveredIssues { json } => uncovered_issues_mode(json),
        Cmd::FlagBlockedDeploy {
            slug,
            pr,
            reason,
            dry_run,
        } => flag_state_mode(&slug, &pr, "ai:blocked-deploy", &reason.join(" "), dry_run),
        Cmd::FlagBlockedInfra {
            slug,
            pr,
            reason,
            dry_run,
        } => flag_state_mode(&slug, &pr, "ai:blocked-infra", &reason.join(" "), dry_run),
        Cmd::FlagBlockedOn {
            slug,
            pr,
            reason,
            dry_run,
        } => flag_state_mode(&slug, &pr, "ai:blocked-on", &reason.join(" "), dry_run),
        Cmd::FlagDesign {
            slug,
            pr,
            reason,
            dry_run,
        } => flag_state_mode(&slug, &pr, "ai:design", &reason.join(" "), dry_run),
        Cmd::ReworkedReject { slug, pr, dry_run } => reworked_reject_mode(&slug, &pr, dry_run),
        Cmd::HumanQueue { json } => human_queue_mode(json),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn close_candidate_plan_respects_state_human_and_dedup() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            close_candidate_plan("CLOSED", &s(&[]), false),
            CloseFlagPlan::AlreadyClosed
        );
        assert_eq!(
            close_candidate_plan("OPEN", &s(&["human:keep-open"]), false),
            CloseFlagPlan::RefuseHuman
        );
        assert_eq!(
            close_candidate_plan("OPEN", &s(&["human:close-candidate"]), false),
            CloseFlagPlan::RefuseHuman
        );
        assert_eq!(
            close_candidate_plan("OPEN", &s(&[]), false),
            CloseFlagPlan::Flag {
                add_label: true,
                post_comment: true
            }
        );
        assert_eq!(
            close_candidate_plan("OPEN", &s(&["ai:close-candidate"]), true),
            CloseFlagPlan::Flag {
                add_label: false,
                post_comment: false
            }
        );
    }

    #[test]
    fn producer_state_plan_guards_human_and_dedups() {
        let body = "🤖 ai:producer\nBlocked-infra: missing FLARE_RPC_URL";
        // human:* label -> refuse
        let j = json!({"labels":[{"name":"human:reject"}],"comments":[],"reviewDecision":null});
        assert_eq!(
            producer_state_plan(&j, "ai:blocked-infra", body),
            ProducerStatePlan::RefuseHuman
        );
        // native human review -> refuse
        let j = json!({"labels":[],"comments":[],"reviewDecision":"APPROVED"});
        assert_eq!(
            producer_state_plan(&j, "ai:blocked-infra", body),
            ProducerStatePlan::RefuseHuman
        );
        // clean, carries a sibling ai:ready -> strip it, add target, post comment
        let j = json!({"labels":[{"name":"ai:ready"}],"comments":[],"reviewDecision":null});
        assert_eq!(
            producer_state_plan(&j, "ai:blocked-infra", body),
            ProducerStatePlan::Flag {
                to_remove: vec!["ai:ready".to_string()],
                has_target: false,
                skip_comment: false,
            }
        );
        // already flagged + identical trusted note present -> no-op (has_target, skip_comment)
        let j = json!({
            "labels":[{"name":"ai:blocked-infra"}],
            "comments":[{"author":{"login":"thedavidmeister"},"body":body}],
            "reviewDecision":null
        });
        assert_eq!(
            producer_state_plan(&j, "ai:blocked-infra", body),
            ProducerStatePlan::Flag {
                to_remove: vec![],
                has_target: true,
                skip_comment: true,
            }
        );
        // a spoofed note from an UNtrusted author does not dedup (still posts)
        let j = json!({
            "labels":[],
            "comments":[{"author":{"login":"impostor"},"body":body}],
            "reviewDecision":null
        });
        assert_eq!(
            producer_state_plan(&j, "ai:blocked-infra", body),
            ProducerStatePlan::Flag {
                to_remove: vec![],
                has_target: false,
                skip_comment: false,
            }
        );
    }

    #[test]
    fn ai_state_label_finds_first_ai_label() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ai_state_label(&s(&["human:x", "ai:blocked-on", "misc"])),
            Some("ai:blocked-on".to_string())
        );
        assert_eq!(ai_state_label(&s(&["human:x", "misc"])), None);
    }

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
    use super::{is_mutation_tool, iso_to_epoch_ms, run_metrics};
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
    // A `user` event (a tool result) carrying the only timestamp in the stream.
    fn user_line(ts: &str) -> String {
        json!({"type":"user","timestamp":ts,"message":{"content":[]}}).to_string()
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
    fn iso_to_epoch_ms_parses_known_timestamps() {
        assert_eq!(iso_to_epoch_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            iso_to_epoch_ms("2026-07-05T09:02:04.035Z"),
            Some(1783242124035)
        );
        // no fractional part → :00.000; and a date the days-from-civil math must get right
        assert_eq!(iso_to_epoch_ms("2000-03-01T00:00:00Z"), Some(951868800000));
    }

    #[test]
    fn iso_to_epoch_ms_rejects_malformed() {
        assert_eq!(iso_to_epoch_ms(""), None);
        assert_eq!(iso_to_epoch_ms("2026-07-05"), None); // no time
        assert_eq!(iso_to_epoch_ms("2026/07/05T09:02:04Z"), None); // wrong separators
        assert_eq!(iso_to_epoch_ms("2026-13-05T09:02:04Z"), None); // month out of range
        assert_eq!(iso_to_epoch_ms("not-a-timestamp-at-all"), None);
    }

    #[test]
    fn startup_ms_is_first_ts_to_first_mutation_result() {
        // reads (with their result timestamps) then the first mutation, whose result timestamp
        // closes the startup window. Only `user` events carry timestamps.
        let trace = [
            tool_line("Bash", "gh search prs --owner x"), // startup read
            user_line("2026-07-05T09:00:00.000Z"),        // FIRST ts → run-start anchor
            tool_line("Bash", "gh pr view 1 --json state"), // startup read
            user_line("2026-07-05T09:00:05.000Z"),
            tool_line("Bash", "gh pr create -R x"), // FIRST MUTATION
            user_line("2026-07-05T09:00:12.500Z"),  // its result → closes the window (+12.5s)
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.first_mutation_index, Some(2));
        assert_eq!(m.startup_ms, Some(12500));
    }

    #[test]
    fn startup_ms_crosses_a_day_boundary() {
        let trace = [
            tool_line("Bash", "gh search prs"),
            user_line("2026-07-05T23:59:59.500Z"), // anchor, late in the day
            tool_line("Bash", "git commit -m x"),  // first mutation
            user_line("2026-07-06T00:00:01.500Z"), // result, next day (+2s)
        ]
        .join("\n");
        assert_eq!(run_metrics(&trace).startup_ms, Some(2000));
    }

    #[test]
    fn startup_ms_is_none_without_a_mutation() {
        let trace = [
            tool_line("Bash", "gh search prs"),
            user_line("2026-07-05T09:00:00.000Z"),
            tool_line("Bash", "gh pr view 1 --json state"),
            user_line("2026-07-05T09:00:05.000Z"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.first_mutation_index, None);
        assert_eq!(m.startup_ms, None);
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
    use super::{
        gc_decision, nix_gc_args, parse_pr_state, parse_repo_slug, should_nix_gc, CloneState,
        GcAction, PrState,
    };

    fn st(clean: bool, unpushed: Option<u32>, pr: Option<PrState>, age_days: u64) -> CloneState {
        CloneState {
            clean,
            unpushed,
            pr,
            age_days,
        }
    }

    #[test]
    fn nix_gc_args_adds_dry_run_only_when_previewing() {
        assert_eq!(nix_gc_args(false), vec!["-d"]);
        assert_eq!(nix_gc_args(true), vec!["-d", "--dry-run"]);
    }

    // The nix store is collected only under disk pressure: at/above the threshold, GC. Strictly
    // below, keep the cache warm. When usage is unknown (None), GC for safety — a possibly-full
    // disk is the worse outcome than a cold cache.
    #[test]
    fn should_nix_gc_gates_on_threshold_and_fails_safe() {
        // Below threshold → skip (keep cache warm).
        assert!(!should_nix_gc(Some(64), 85));
        assert!(!should_nix_gc(Some(84), 85));
        // At the threshold → collect (boundary is inclusive).
        assert!(should_nix_gc(Some(85), 85));
        // Above threshold → collect.
        assert!(should_nix_gc(Some(90), 85));
        assert!(should_nix_gc(Some(100), 85));
        // Unknown usage → collect for safety.
        assert!(should_nix_gc(None, 85));
        // A 0 threshold always collects; even at 0% usage 0 >= 0 holds.
        assert!(should_nix_gc(Some(0), 0));
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

#[cfg(test)]
mod deploy_tests {
    use super::{
        build_dispatch_inputs, classify_run, dispatch_command, parse_dispatch_inputs,
        pick_selector, RunResult, WorkflowInput,
    };

    // The real rain.erc4626.words workflow: a single `network` choice input, one option `base`.
    const NETWORK_WF: &str = r#"name: Manual sol artifacts
on:
  workflow_dispatch:
    inputs:
      network:
        description: 'Network to deploy to'
        required: true
        type: choice
        options:
          - base
jobs:
  deploy:
    runs-on: ubuntu-latest
"#;

    // The real raindex workflow: a single `suite` choice input with several options.
    const SUITE_WF: &str = r#"name: Manual sol artifacts
on:
  workflow_dispatch:
    inputs:
      suite:
        description: "Suite to deploy"
        required: true
        type: choice
        options:
          - raindex
          - subparser
          - route-processor
jobs:
  deploy:
    uses: rainlanguage/rainix/.github/workflows/rainix-manual-sol-artifacts.yaml@main
    with:
      suite: ${{ inputs.suite }}
    secrets: inherit
"#;

    // A hypothetical two-input workflow (selector + a second required input carrying a default).
    const TWO_INPUT_WF: &str = r#"on:
  workflow_dispatch:
    inputs:
      network:
        required: true
        type: choice
        options:
          - base
          - flare
      dry_run:
        required: true
        default: "false"
jobs: {}
"#;

    // No workflow_dispatch at all → no inputs.
    const NO_DISPATCH_WF: &str = r#"name: CI
on:
  push:
    branches: [main]
jobs: {}
"#;

    // --- parse_dispatch_inputs ------------------------------------------------------------------

    #[test]
    fn parses_single_network_input() {
        let got = parse_dispatch_inputs(NETWORK_WF);
        assert_eq!(
            got,
            vec![WorkflowInput {
                name: "network".to_string(),
                required: true,
                default: None,
                options: vec!["base".to_string()],
            }]
        );
    }

    #[test]
    fn parses_suite_with_multiple_options() {
        let got = parse_dispatch_inputs(SUITE_WF);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "suite");
        assert!(got[0].required);
        assert_eq!(
            got[0].options,
            vec!["raindex", "subparser", "route-processor"]
        );
        // The later `with:\n  suite:` block must NOT be mistaken for a second input.
        assert_eq!(
            got.len(),
            1,
            "only the dispatch input, not the with: mapping"
        );
    }

    #[test]
    fn parses_two_inputs_with_default() {
        let got = parse_dispatch_inputs(TWO_INPUT_WF);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "network");
        assert_eq!(got[0].options, vec!["base", "flare"]);
        assert_eq!(got[1].name, "dry_run");
        assert_eq!(got[1].default.as_deref(), Some("false"));
        assert!(got[1].options.is_empty());
    }

    #[test]
    fn no_dispatch_block_yields_no_inputs() {
        assert!(parse_dispatch_inputs(NO_DISPATCH_WF).is_empty());
        assert!(parse_dispatch_inputs("").is_empty());
    }

    // --- pick_selector --------------------------------------------------------------------------

    #[test]
    fn selector_prefers_a_named_selector_then_sole_input() {
        let net = parse_dispatch_inputs(NETWORK_WF);
        assert_eq!(pick_selector(&net), Some(0));
        let suite = parse_dispatch_inputs(SUITE_WF);
        assert_eq!(pick_selector(&suite), Some(0), "sole input is the selector");
        let two = parse_dispatch_inputs(TWO_INPUT_WF);
        assert_eq!(
            pick_selector(&two),
            Some(0),
            "`network` wins over `dry_run`"
        );
        // Two inputs, neither a selector-name → ambiguous.
        let ambiguous = vec![
            WorkflowInput {
                name: "foo".into(),
                required: true,
                default: None,
                options: vec![],
            },
            WorkflowInput {
                name: "bar".into(),
                required: true,
                default: None,
                options: vec![],
            },
        ];
        assert_eq!(pick_selector(&ambiguous), None);
    }

    // --- build_dispatch_inputs ------------------------------------------------------------------

    // Single-option selector, no --network → auto-picks the sole option (the erc4626.words case).
    #[test]
    fn builds_single_option_selector_without_network() {
        let decl = parse_dispatch_inputs(NETWORK_WF);
        assert_eq!(
            build_dispatch_inputs(&decl, None).unwrap(),
            vec![("network".to_string(), "base".to_string())]
        );
        // Explicit --network base is identical; a non-option value is rejected.
        assert_eq!(
            build_dispatch_inputs(&decl, Some("base")).unwrap(),
            vec![("network".to_string(), "base".to_string())]
        );
        assert!(build_dispatch_inputs(&decl, Some("arbitrum")).is_err());
    }

    // Multi-option selector with no default REQUIRES --network (never guess among options).
    #[test]
    fn multi_option_selector_requires_network() {
        let decl = parse_dispatch_inputs(SUITE_WF);
        assert!(
            build_dispatch_inputs(&decl, None).is_err(),
            "must not guess among several suites"
        );
        assert_eq!(
            build_dispatch_inputs(&decl, Some("subparser")).unwrap(),
            vec![("suite".to_string(), "subparser".to_string())]
        );
        assert!(build_dispatch_inputs(&decl, Some("nonsuch")).is_err());
    }

    // Selector filled by --network; the OTHER required input filled from its default.
    #[test]
    fn fills_non_selector_required_from_default() {
        let decl = parse_dispatch_inputs(TWO_INPUT_WF);
        assert_eq!(
            build_dispatch_inputs(&decl, Some("flare")).unwrap(),
            vec![
                ("network".to_string(), "flare".to_string()),
                ("dry_run".to_string(), "false".to_string()),
            ]
        );
    }

    // No declared inputs → empty dispatch; but --network with no inputs is an error.
    #[test]
    fn no_inputs_dispatch_is_empty_and_rejects_network() {
        assert!(build_dispatch_inputs(&[], None).unwrap().is_empty());
        assert!(build_dispatch_inputs(&[], Some("base")).is_err());
    }

    // Ambiguous multi-input workflow + --network → error rather than a wrong deploy.
    #[test]
    fn ambiguous_selector_with_network_errors() {
        let ambiguous = vec![
            WorkflowInput {
                name: "foo".into(),
                required: true,
                default: Some("x".into()),
                options: vec![],
            },
            WorkflowInput {
                name: "bar".into(),
                required: true,
                default: Some("y".into()),
                options: vec![],
            },
        ];
        assert!(build_dispatch_inputs(&ambiguous, Some("base")).is_err());
    }

    // --- dispatch_command -----------------------------------------------------------------------

    #[test]
    fn dispatch_command_builds_the_gh_argv() {
        let inputs = vec![("network".to_string(), "base".to_string())];
        assert_eq!(
            dispatch_command(
                "manual-sol-artifacts.yaml",
                "rainlanguage/rain.erc4626.words",
                "my-branch",
                &inputs
            ),
            vec![
                "gh",
                "workflow",
                "run",
                "manual-sol-artifacts.yaml",
                "-R",
                "rainlanguage/rain.erc4626.words",
                "--ref",
                "my-branch",
                "-f",
                "network=base",
            ]
        );
        // No inputs → no -f flags.
        assert_eq!(
            dispatch_command("f.yml", "o/r", "b", &[]),
            vec!["gh", "workflow", "run", "f.yml", "-R", "o/r", "--ref", "b"]
        );
    }

    // --- classify_run ---------------------------------------------------------------------------

    #[test]
    fn classify_run_is_terminal_only_when_completed() {
        assert_eq!(
            classify_run(Some("completed"), Some("success")),
            RunResult::Success
        );
        for c in [
            "failure",
            "cancelled",
            "timed_out",
            "action_required",
            "startup_failure",
        ] {
            assert_eq!(
                classify_run(Some("completed"), Some(c)),
                RunResult::Failure,
                "conclusion {c} is a failure"
            );
        }
        // Completed with no conclusion is not a success → Failure (never a false green).
        assert_eq!(classify_run(Some("completed"), None), RunResult::Failure);
        // Anything not-yet-completed is InProgress regardless of conclusion.
        for s in ["queued", "in_progress", "waiting", "requested", "pending"] {
            assert_eq!(
                classify_run(Some(s), None),
                RunResult::InProgress,
                "status {s} is in progress"
            );
        }
        assert_eq!(classify_run(None, None), RunResult::InProgress);
    }
}

// Pin the clap arg surface: every subcommand's name, positional ORDER, flags, and defaults, so a
// silent regression in the derive (a dropped subcommand, a swapped positional, a renamed/lost flag,
// a changed default, or the note/reason Vec swallowing a flag) fails the suite. Parses via the public
// `Cli`, exactly as `main` does, so these assert the real dispatch contract.
#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cmd {
        Cli::try_parse_from(args)
            .unwrap_or_else(|e| panic!("expected {args:?} to parse: {e}"))
            .command
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // All 9 subcommands are present and dispatch to the right variant on their kebab-case name.
    #[test]
    fn all_nine_subcommands_present() {
        assert!(matches!(parse(&["prr", "queue"]), Cmd::Queue { .. }));
        assert!(matches!(
            parse(&["prr", "record-verdict", "o/r", "1", "ready"]),
            Cmd::RecordVerdict { .. }
        ));
        assert!(matches!(
            parse(&["prr", "flag-close-candidate", "o/r", "1"]),
            Cmd::FlagCloseCandidate { .. }
        ));
        assert!(matches!(
            parse(&["prr", "trusted-comments", "o/r", "1"]),
            Cmd::TrustedComments { .. }
        ));
        assert!(matches!(
            parse(&["prr", "commit-closes", "o/r", "1"]),
            Cmd::CommitCloses { .. }
        ));
        assert!(matches!(
            parse(&["prr", "deploy", "o/r", "1"]),
            Cmd::Deploy { .. }
        ));
        assert!(matches!(
            parse(&["prr", "gc-clones", "/w"]),
            Cmd::GcClones { .. }
        ));
        assert!(matches!(parse(&["prr", "gc", "/w"]), Cmd::Gc { .. }));
        assert!(matches!(
            parse(&["prr", "run-metrics", "t.jsonl"]),
            Cmd::RunMetrics { .. }
        ));
    }

    #[test]
    fn fsm_state_subcommands_present() {
        assert!(matches!(
            parse(&[
                "prr",
                "flag-blocked-deploy",
                "o/r",
                "1",
                "run",
                "28",
                "failed"
            ]),
            Cmd::FlagBlockedDeploy { .. }
        ));
        assert!(matches!(
            parse(&["prr", "flag-blocked-infra", "o/r", "1", "missing", "secret"]),
            Cmd::FlagBlockedInfra { .. }
        ));
        assert!(matches!(
            parse(&["prr", "flag-blocked-on", "o/r", "1", "waiting", "on", "#9"]),
            Cmd::FlagBlockedOn { .. }
        ));
        assert!(matches!(
            parse(&["prr", "flag-design", "o/r", "1", "version", "slot", "taken"]),
            Cmd::FlagDesign { .. }
        ));
        assert!(matches!(
            parse(&["prr", "reworked-reject", "o/r", "1"]),
            Cmd::ReworkedReject { .. }
        ));
        assert_eq!(
            parse(&["prr", "reworked-reject", "o/r", "1", "--dry-run"]),
            Cmd::ReworkedReject {
                slug: "o/r".to_string(),
                pr: "1".to_string(),
                dry_run: true,
            }
        );
        assert!(matches!(
            parse(&["prr", "human-queue"]),
            Cmd::HumanQueue { .. }
        ));
    }

    // The reason is variadic + joined; --dry-run is a flag, not swallowed into the reason.
    #[test]
    fn flag_blocked_reason_is_variadic_and_dry_run_is_a_flag() {
        assert_eq!(
            parse(&[
                "prr",
                "flag-blocked-infra",
                "o/r",
                "1",
                "missing",
                "FLARE_RPC_URL",
                "--dry-run"
            ]),
            Cmd::FlagBlockedInfra {
                slug: "o/r".to_string(),
                pr: "1".to_string(),
                reason: s(&["missing", "FLARE_RPC_URL"]),
                dry_run: true,
            }
        );
    }

    // queue: N is an optional usize. Omitted → None (so `main`'s `unwrap_or(20)` supplies the 20);
    // given → Some(N). A clap-level default slipped onto `n` would make the omitted case Some and
    // fail here.
    #[test]
    fn queue_n_is_optional() {
        assert_eq!(parse(&["prr", "queue"]), Cmd::Queue { n: None });
        assert_eq!(parse(&["prr", "queue", "5"]), Cmd::Queue { n: Some(5) });
    }

    // record-verdict positional ORDER: slug, then pr, then verdict. A swap of any two is a silent,
    // severe bug (records against the wrong PR / label) — this pins the exact binding.
    #[test]
    fn record_verdict_positional_order() {
        let c = parse(&["prr", "record-verdict", "owner/repo", "42", "ready"]);
        assert_eq!(
            c,
            Cmd::RecordVerdict {
                slug: "owner/repo".to_string(),
                pr: "42".to_string(),
                verdict: "ready".to_string(),
                note: vec![],
                cost: None,
                basis: String::new(),
                dry_run: false,
            }
        );
    }

    // The highest-risk spot: the trailing `note: Vec<String>` joins multi-word notes AND does NOT
    // swallow the flags that follow it. A note followed by MULTIPLE flags must still bind each flag.
    #[test]
    fn record_verdict_note_joins_and_does_not_swallow_flags() {
        let c = parse(&[
            "prr",
            "record-verdict",
            "o/r",
            "5",
            "ready",
            "my",
            "note",
            "here",
            "--cost",
            "100",
            "--basis",
            "org gate",
            "--dry-run",
        ]);
        assert_eq!(
            c,
            Cmd::RecordVerdict {
                slug: "o/r".to_string(),
                pr: "5".to_string(),
                verdict: "ready".to_string(),
                note: s(&["my", "note", "here"]),
                cost: Some(100),
                basis: "org gate".to_string(),
                dry_run: true,
            }
        );
        // and the note joins to the exact string main forwards to record_verdict_mode
        if let Cmd::RecordVerdict { note, .. } = c {
            assert_eq!(note.join(" "), "my note here");
        }
    }

    // An EMPTY note followed immediately by flags: note is [], flags still bind.
    #[test]
    fn record_verdict_empty_note_with_flags() {
        let c = parse(&[
            "prr",
            "record-verdict",
            "o/r",
            "5",
            "ready",
            "--cost",
            "5",
            "--dry-run",
        ]);
        assert_eq!(
            c,
            Cmd::RecordVerdict {
                slug: "o/r".to_string(),
                pr: "5".to_string(),
                verdict: "ready".to_string(),
                note: vec![],
                cost: Some(5),
                basis: String::new(),
                dry_run: true,
            }
        );
    }

    // record-verdict defaults with no flags: cost None, basis "" (the pinned default), dry_run false.
    #[test]
    fn record_verdict_flag_defaults() {
        let c = parse(&["prr", "record-verdict", "o/r", "5", "reject", "bad"]);
        assert_eq!(
            c,
            Cmd::RecordVerdict {
                slug: "o/r".to_string(),
                pr: "5".to_string(),
                verdict: "reject".to_string(),
                note: s(&["bad"]),
                cost: None,
                basis: String::new(),
                dry_run: false,
            }
        );
    }

    // flag-close-candidate: slug, issue, then the trailing reason Vec; --dry-run does not get eaten.
    #[test]
    fn flag_close_candidate_reason_and_dry_run() {
        assert_eq!(
            parse(&[
                "prr",
                "flag-close-candidate",
                "o/r",
                "7",
                "dup",
                "of",
                "#3",
                "--dry-run",
            ]),
            Cmd::FlagCloseCandidate {
                slug: "o/r".to_string(),
                issue: "7".to_string(),
                reason: s(&["dup", "of", "#3"]),
                dry_run: true,
            }
        );
        // empty reason is allowed at the parse layer (mode-level guard rejects it, not clap)
        assert_eq!(
            parse(&["prr", "flag-close-candidate", "o/r", "7"]),
            Cmd::FlagCloseCandidate {
                slug: "o/r".to_string(),
                issue: "7".to_string(),
                reason: vec![],
                dry_run: false,
            }
        );
    }

    // trusted-comments: slug, n; --marker takes a value, --issue is a bare bool.
    #[test]
    fn trusted_comments_marker_and_issue() {
        assert_eq!(
            parse(&[
                "prr",
                "trusted-comments",
                "o/r",
                "9",
                "--marker",
                "🤖 ai:vetter",
                "--issue",
            ]),
            Cmd::TrustedComments {
                slug: "o/r".to_string(),
                n: "9".to_string(),
                marker: Some("🤖 ai:vetter".to_string()),
                issue: true,
            }
        );
        assert_eq!(
            parse(&["prr", "trusted-comments", "o/r", "9"]),
            Cmd::TrustedComments {
                slug: "o/r".to_string(),
                n: "9".to_string(),
                marker: None,
                issue: false,
            }
        );
    }

    #[test]
    fn commit_closes_order() {
        assert_eq!(
            parse(&["prr", "commit-closes", "owner/repo", "88"]),
            Cmd::CommitCloses {
                slug: "owner/repo".to_string(),
                pr: "88".to_string(),
            }
        );
    }

    #[test]
    fn deploy_network_and_dry_run() {
        assert_eq!(
            parse(&[
                "prr",
                "deploy",
                "o/r",
                "12",
                "--network",
                "base",
                "--dry-run"
            ]),
            Cmd::Deploy {
                slug: "o/r".to_string(),
                pr: "12".to_string(),
                network: Some("base".to_string()),
                dry_run: true,
            }
        );
        assert_eq!(
            parse(&["prr", "deploy", "o/r", "12"]),
            Cmd::Deploy {
                slug: "o/r".to_string(),
                pr: "12".to_string(),
                network: None,
                dry_run: false,
            }
        );
    }

    // gc-clones: work-dir is required; --max-age-days defaults to 30 (the pinned default).
    #[test]
    fn gc_clones_defaults_and_flags() {
        assert_eq!(
            parse(&["prr", "gc-clones", "/w"]),
            Cmd::GcClones {
                work_dir: "/w".to_string(),
                dry_run: false,
                max_age_days: 30,
            }
        );
        assert_eq!(
            parse(&["prr", "gc-clones", "/w", "--dry-run", "--max-age-days", "7"]),
            Cmd::GcClones {
                work_dir: "/w".to_string(),
                dry_run: true,
                max_age_days: 7,
            }
        );
        // work-dir is mandatory for gc-clones (unlike gc); omitting it is a parse error.
        assert!(Cli::try_parse_from(["prr", "gc-clones"]).is_err());
    }

    // gc: work-dir is OPTIONAL at the parse layer (the required-unless-`--no-clones` rule is enforced
    // in main, after parsing). --max-age-days defaults to 30; --no-clones/--no-nix are bare bools.
    #[test]
    fn gc_workdir_optional_defaults_and_bools() {
        assert_eq!(
            parse(&["prr", "gc", "/w"]),
            Cmd::Gc {
                work_dir: Some("/w".to_string()),
                dry_run: false,
                max_age_days: 30,
                no_clones: false,
                no_nix: false,
                nix_threshold: 85,
            }
        );
        // --no-clones with NO work-dir must still parse (main then allows it); this is the parse-layer
        // precondition of the "required unless --no-clones" rule.
        assert_eq!(
            parse(&["prr", "gc", "--no-clones", "--no-nix"]),
            Cmd::Gc {
                work_dir: None,
                dry_run: false,
                max_age_days: 30,
                no_clones: true,
                no_nix: true,
                nix_threshold: 85,
            }
        );
        assert_eq!(
            parse(&[
                "prr",
                "gc",
                "/w",
                "--dry-run",
                "--max-age-days",
                "5",
                "--no-nix"
            ]),
            Cmd::Gc {
                work_dir: Some("/w".to_string()),
                dry_run: true,
                max_age_days: 5,
                no_clones: false,
                no_nix: true,
                nix_threshold: 85,
            }
        );
        // --nix-threshold overrides the 85 default.
        assert_eq!(
            parse(&["prr", "gc", "/w", "--nix-threshold", "50"]),
            Cmd::Gc {
                work_dir: Some("/w".to_string()),
                dry_run: false,
                max_age_days: 30,
                no_clones: false,
                no_nix: false,
                nix_threshold: 50,
            }
        );
    }

    #[test]
    fn run_metrics_trace() {
        assert_eq!(
            parse(&["prr", "run-metrics", "/path/to/trace.jsonl"]),
            Cmd::RunMetrics {
                trace: "/path/to/trace.jsonl".to_string(),
            }
        );
    }

    // The pre-conversion `--foo` dispatch forms are gone: clap must REJECT them as unknown args
    // (this is the intended, correct new behavior — callers were migrated to the bare subcommand).
    #[test]
    fn old_dashed_dispatch_forms_are_rejected() {
        for old in [
            vec!["prr", "--queue"],
            vec!["prr", "--record-verdict", "o/r", "1", "ready"],
            vec!["prr", "--deploy", "o/r", "1"],
            vec!["prr", "--gc", "/w"],
        ] {
            assert!(
                Cli::try_parse_from(&old).is_err(),
                "old form {old:?} must be rejected"
            );
        }
    }
}

#[cfg(test)]
mod worklist_tests {
    use super::*;
    use serde_json::json;

    fn sig(ci: Ci, merge: &str) -> PrSignals {
        PrSignals {
            ci,
            merge_state: merge.to_string(),
            unresolved_threads: 0,
            has_deploy_trigger: false,
            deploy_done_at_head: false,
            parked: false,
            ui_missing_screenshot: false,
            state_label: None,
        }
    }

    #[test]
    fn modeled_state_label_short_circuits_to_parked() {
        // A PR already in a human-gated state is parked for the human regardless of CI — even a
        // deploy-trigger or a red-green signal does not override the label.
        for label in [
            "ai:design",
            "ai:blocked-deploy",
            "ai:blocked-infra",
            "ai:blocked-on",
            "ai:close-candidate",
        ] {
            let mut s = sig(Ci::Green, "CLEAN");
            s.state_label = Some(label.to_string());
            s.has_deploy_trigger = true; // would otherwise be Deploy
            assert_eq!(
                next_action(&s),
                NextAction::ParkedSkip,
                "label {label} should park"
            );
        }
        // ai:ready is NOT a producer human-gated block — it classifies from CI as normal.
        let mut s = sig(Ci::Green, "CLEAN");
        s.state_label = Some("ai:ready".to_string());
        assert_eq!(next_action(&s), NextAction::GreenReady);
    }

    #[test]
    fn green_clean_is_green_ready() {
        assert_eq!(
            next_action(&sig(Ci::Green, "CLEAN")),
            NextAction::GreenReady
        );
        // BLOCKED = green but needs human approval -> still present it to the human.
        assert_eq!(
            next_action(&sig(Ci::Green, "BLOCKED")),
            NextAction::GreenReady
        );
    }

    #[test]
    fn red_unparked_is_needs3b_parked_is_skip() {
        assert_eq!(next_action(&sig(Ci::Red, "BLOCKED")), NextAction::Needs3b);
        let mut s = sig(Ci::Red, "BLOCKED");
        s.parked = true;
        assert_eq!(next_action(&s), NextAction::ParkedSkip);
    }

    #[test]
    fn deploy_trigger_leads_even_when_green() {
        let mut s = sig(Ci::Green, "CLEAN");
        s.has_deploy_trigger = true;
        assert_eq!(next_action(&s), NextAction::Deploy);
        // ...unless the deploy already succeeded at head -> back to green-ready.
        s.deploy_done_at_head = true;
        assert_eq!(next_action(&s), NextAction::GreenReady);
    }

    #[test]
    fn conflict_and_threads_and_screenshot_route() {
        assert_eq!(
            next_action(&sig(Ci::Green, "DIRTY")),
            NextAction::Conflict3d
        );
        assert_eq!(
            next_action(&sig(Ci::Green, "BEHIND")),
            NextAction::Conflict3d
        );
        let mut s = sig(Ci::Green, "CLEAN");
        s.unresolved_threads = 2;
        assert_eq!(next_action(&s), NextAction::Coderabbit3e);
        let mut s = sig(Ci::Green, "CLEAN");
        s.ui_missing_screenshot = true;
        assert_eq!(next_action(&s), NextAction::Screenshot3c);
    }

    #[test]
    fn pending_ci_waits() {
        assert_eq!(next_action(&sig(Ci::Pending, "UNKNOWN")), NextAction::Wait);
    }

    #[test]
    fn failing_check_names_picks_only_failures() {
        let rollup = json!([
            {"name":"a","conclusion":"SUCCESS"},
            {"name":"b","conclusion":"FAILURE"},
            {"context":"c","state":"ERROR"},
            {"name":"d","status":"IN_PROGRESS"},
        ]);
        let mut got = failing_check_names(&rollup);
        got.sort();
        assert_eq!(got, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn failing_check_names_catches_every_failure_conclusion() {
        // Every failing conclusion/state must be caught — not just FAILURE/ERROR. A mutation that
        // drops any of TIMED_OUT/CANCELLED/ACTION_REQUIRED/STARTUP_FAILURE fails here.
        let rollup = json!([
            {"name":"f1","conclusion":"FAILURE"},
            {"name":"f2","conclusion":"TIMED_OUT"},
            {"name":"f3","conclusion":"CANCELLED"},
            {"name":"f4","conclusion":"ACTION_REQUIRED"},
            {"name":"f5","conclusion":"STARTUP_FAILURE"},
            {"context":"f6","state":"ERROR"},
            {"name":"ok","conclusion":"SUCCESS"},
            {"name":"pend","status":"IN_PROGRESS"},
        ]);
        let mut got = failing_check_names(&rollup);
        got.sort();
        assert_eq!(
            got,
            ["f1", "f2", "f3", "f4", "f5", "f6"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_single_unresolved_thread_routes_to_coderabbit() {
        // The threshold is > 0, not > 1: ONE open thread already routes to coderabbit-3e.
        let mut s = sig(Ci::Green, "CLEAN");
        s.unresolved_threads = 1;
        assert_eq!(next_action(&s), NextAction::Coderabbit3e);
    }

    #[test]
    fn green_branch_precedence_is_conflict_then_threads_then_screenshot() {
        // conflict wins over open threads AND a missing screenshot
        let mut s = sig(Ci::Green, "DIRTY");
        s.unresolved_threads = 3;
        s.ui_missing_screenshot = true;
        assert_eq!(next_action(&s), NextAction::Conflict3d);
        // with no conflict, open threads win over a missing screenshot
        let mut s = sig(Ci::Green, "CLEAN");
        s.unresolved_threads = 2;
        s.ui_missing_screenshot = true;
        assert_eq!(next_action(&s), NextAction::Coderabbit3e);
        // screenshot is last
        let mut s = sig(Ci::Green, "CLEAN");
        s.ui_missing_screenshot = true;
        assert_eq!(next_action(&s), NextAction::Screenshot3c);
    }

    #[test]
    fn ai_state_label_returns_the_first_when_two_slip_in() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            ai_state_label(&s(&["ai:design", "ai:ready"])),
            Some("ai:design".to_string())
        );
        assert_eq!(
            ai_state_label(&s(&["ai:ready", "ai:blocked-infra"])),
            Some("ai:ready".to_string())
        );
    }

    // --- worklist_row: the untested integration seam (pure — reads everything from `detail`) ------

    #[test]
    fn worklist_row_deploy_done_must_be_head_scoped() {
        // A deploy-confirmed note at a PRIOR head (HEAD_A) must NOT mark the current head (HEAD_B)
        // done: the PR pushed new bytecode (REQUIRES redeploy) and still needs the redeploy. Under
        // the dropped un-head-scoped clause this returned green-ready with undeployed bytecode.
        let detail = json!({
            "number": 7, "url": "", "title": "t", "headRefOid": "HEAD_B",
            "body": "REQUIRES redeploy at land",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [],
            "comments": [{"author":{"login":"thedavidmeister"},
                          "body":"🤖 ai:producer deploy-confirmed at HEAD_A"}]
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "deploy");
        // ...and WITH the note at the current head, the deploy IS done → green-ready.
        let detail = json!({
            "number": 7, "url": "", "title": "t", "headRefOid": "HEAD_B",
            "body": "REQUIRES redeploy at land",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [],
            "comments": [{"author":{"login":"thedavidmeister"},
                          "body":"🤖 ai:producer deploy-confirmed at HEAD_B"}]
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "green-ready");
    }

    #[test]
    fn worklist_row_red_prodpin_is_deploy() {
        let detail = json!({
            "number": 1, "headRefOid": "H",
            "statusCheckRollup": [{"name":"rainix-sol / test / testProdDeployArbitrum",
                                   "conclusion":"FAILURE","status":"COMPLETED"}],
            "mergeStateStatus": "BLOCKED", "labels": [], "comments": []
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "deploy");
    }

    #[test]
    fn worklist_row_requires_redeploy_green_is_deploy() {
        let detail = json!({
            "number": 1, "headRefOid": "H", "body": "REQUIRES redeploy at land",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [], "comments": []
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "deploy");
    }

    #[test]
    fn worklist_row_still_red_handed_off_is_parked() {
        // A red PR carrying a trusted hand-off note is parked — the producer does not re-touch it.
        let detail = json!({
            "number": 1, "headRefOid": "H",
            "statusCheckRollup": [{"name":"unit","conclusion":"FAILURE","status":"COMPLETED"}],
            "mergeStateStatus": "BLOCKED", "labels": [],
            "comments": [{"author":{"login":"thedavidmeister"},
                          "body":"🤖 ai:producer HAND OFF: infra red"}]
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "parked-skip");
    }

    #[test]
    fn worklist_row_ui_missing_screenshot_routes() {
        let detail = json!({
            "number": 5, "headRefOid": "H",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [], "comments": [],
            "files": [{"path":"packages/webapp/src/Foo.svelte"}]
        });
        assert_eq!(worklist_row("o/r", &detail)["nextAction"], "screenshot-3c");
    }

    #[test]
    fn uncovered_excludes_only_same_repo_covered() {
        use std::collections::HashSet;
        let issues = vec![
            ("o/a".to_string(), 5u64),
            ("o/a".to_string(), 6),
            ("o/b".to_string(), 5),
        ];
        let mut covered = HashSet::new();
        covered.insert(("o/a".to_string(), 5u64)); // covers a#5 only
        let got = uncovered(&issues, &covered);
        assert!(got.contains(&("o/a".to_string(), 6)));
        assert!(got.contains(&("o/b".to_string(), 5))); // same number, different repo -> NOT covered
        assert!(!got.contains(&("o/a".to_string(), 5)));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn cache_hit_only_when_unchanged_terminal_and_fresh() {
        // baseline: same updatedAt, terminal green, within ttl -> HIT
        assert!(cache_fresh("t1", "green", 100, "t1", 200, 10800));
        assert!(cache_fresh("t1", "red", 100, "t1", 200, 10800));
        // updatedAt moved -> MISS
        assert!(!cache_fresh("t1", "green", 100, "t2", 200, 10800));
        // non-terminal ci -> MISS even if unchanged + fresh
        assert!(!cache_fresh("t1", "pending", 100, "t1", 200, 10800));
        assert!(!cache_fresh("t1", "nochecks", 100, "t1", 200, 10800));
        // past ttl -> MISS
        assert!(!cache_fresh("t1", "green", 100, "t1", 100 + 10800, 10800));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FSM-completeness tests: the transient reworked-reject gate + full-inventory lane bucketing.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod fsm_completeness_tests {
    use super::*;
    use serde_json::json;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // --- reworked-reject gate (the pure date comparison) ---------------------------------------

    #[test]
    fn parse_rfc3339_orders_chronologically() {
        // Later timestamp parses to a strictly greater tuple, across every field boundary.
        let base = parse_rfc3339_utc("2026-07-12T10:30:00Z").unwrap();
        assert!(parse_rfc3339_utc("2026-07-12T10:30:01Z").unwrap() > base); // +1s
        assert!(parse_rfc3339_utc("2026-07-12T11:00:00Z").unwrap() > base); // +hour
        assert!(parse_rfc3339_utc("2026-07-13T00:00:00Z").unwrap() > base); // +day
        assert!(parse_rfc3339_utc("2027-01-01T00:00:00Z").unwrap() > base); // +year
        assert!(parse_rfc3339_utc("2026-07-12T10:29:59Z").unwrap() < base); // earlier
                                                                            // Fractional seconds + missing Z are tolerated (the leading Y-M-DTH:M:S is what compares).
        assert_eq!(
            parse_rfc3339_utc("2026-07-12T10:30:00.123Z"),
            Some((2026, 7, 12, 10, 30, 0))
        );
        assert_eq!(parse_rfc3339_utc("not a date"), None);
    }

    #[test]
    fn reworked_reject_clears_only_when_head_strictly_postdates_reject() {
        // Head commit AFTER the reject event -> Clear (a rework provably followed the reject).
        assert_eq!(
            reworked_reject_decision(Some("2026-07-12T10:00:01Z"), Some("2026-07-12T10:00:00Z")),
            ReworkedRejectDecision::Clear
        );
        // Head commit BEFORE the reject -> refuse; the reject stands (this is the dead-end example:
        // a stale head that predates the human reject must NOT clear it).
        assert_eq!(
            reworked_reject_decision(Some("2026-07-12T09:59:59Z"), Some("2026-07-12T10:00:00Z")),
            ReworkedRejectDecision::RefuseNotReworked
        );
        // EQUAL timestamps -> refuse (strict `>`; equality is not "strictly newer", fail safe).
        assert_eq!(
            reworked_reject_decision(Some("2026-07-12T10:00:00Z"), Some("2026-07-12T10:00:00Z")),
            ReworkedRejectDecision::RefuseNotReworked
        );
        // No reject event at all -> nothing to transition.
        assert_eq!(
            reworked_reject_decision(Some("2026-07-12T10:00:01Z"), None),
            ReworkedRejectDecision::RefuseNoReject
        );
        // Unreadable / missing head date -> fail safe, never clear on incomplete data.
        assert_eq!(
            reworked_reject_decision(None, Some("2026-07-12T10:00:00Z")),
            ReworkedRejectDecision::RefuseNoHeadDate
        );
        assert_eq!(
            reworked_reject_decision(Some("garbage"), Some("2026-07-12T10:00:00Z")),
            ReworkedRejectDecision::RefuseNoHeadDate
        );
    }

    #[test]
    fn latest_labeled_event_picks_the_most_recent_matching_label() {
        // Two human:reject applications (removed then re-applied): the LATEST wins; a labeled event
        // for a DIFFERENT label and a non-labeled event are both ignored.
        let events = json!([
            {"event": "labeled",   "label": {"name": "human:reject"}, "created_at": "2026-07-10T08:00:00Z"},
            {"event": "unlabeled", "label": {"name": "human:reject"}, "created_at": "2026-07-11T08:00:00Z"},
            {"event": "labeled",   "label": {"name": "ai:ready"},     "created_at": "2026-07-13T08:00:00Z"},
            {"event": "labeled",   "label": {"name": "human:reject"}, "created_at": "2026-07-12T08:00:00Z"}
        ]);
        assert_eq!(
            latest_labeled_event_date(Some(&events), "human:reject").as_deref(),
            Some("2026-07-12T08:00:00Z")
        );
        // No matching label -> None (RefuseNoReject downstream).
        assert_eq!(
            latest_labeled_event_date(Some(&events), "human:design"),
            None
        );
        assert_eq!(latest_labeled_event_date(None, "human:reject"), None);
    }

    // --- all-state lane bucketing --------------------------------------------------------------

    #[test]
    fn classify_lane_maps_every_state_by_precedence() {
        // human decision dominates a stale ai:* label.
        assert_eq!(
            classify_lane(&s(&["ai:ready", "human:reject"]), Some(true), false),
            (Lane::HumanDecisions, "human:reject".to_string())
        );
        assert_eq!(
            classify_lane(&s(&["human:design"]), None, false),
            (Lane::HumanDecisions, "human:design".to_string())
        );
        // producer-blocked next.
        assert_eq!(
            classify_lane(&s(&["ai:blocked-infra"]), None, false),
            (Lane::ProducerBlocked, "ai:blocked-infra".to_string())
        );
        // ai:ready splits on head drift: vetted-at-head stays ready, moved head -> awaiting-re-vet.
        assert_eq!(
            classify_lane(&s(&["ai:ready"]), Some(true), false),
            (Lane::VetterVerdicts, "ai:ready".to_string())
        );
        assert_eq!(
            classify_lane(&s(&["ai:ready"]), Some(false), false),
            (Lane::VetLifecycle, "awaiting-re-vet".to_string())
        );
        // other vetter verdicts (ai:design is a verdict lane, NOT producer-blocked).
        assert_eq!(
            classify_lane(&s(&["ai:reject"]), None, false),
            (Lane::VetterVerdicts, "ai:reject".to_string())
        );
        assert_eq!(
            classify_lane(&s(&["ai:relink"]), None, false),
            (Lane::VetterVerdicts, "ai:relink".to_string())
        );
        assert_eq!(
            classify_lane(&s(&["ai:design"]), None, false),
            (Lane::VetterVerdicts, "ai:design".to_string())
        );
        assert_eq!(
            classify_lane(&s(&["ai:close-candidate"]), None, false),
            (Lane::VetterVerdicts, "ai:close-candidate".to_string())
        );
        // label-less: leak if the producer commented, else un-vetted.
        assert_eq!(
            classify_lane(&s(&[]), None, true),
            (Lane::Leak, "leak".to_string())
        );
        assert_eq!(
            classify_lane(&s(&[]), None, false),
            (Lane::VetLifecycle, "un-vetted".to_string())
        );
    }

    fn qpr(
        num: u64,
        labels: &[&str],
        ready_vetted_at_head: Option<bool>,
        producer_commented: bool,
    ) -> QueuePr {
        QueuePr {
            repo: "o/r".to_string(),
            number: num,
            title: format!("pr {num}"),
            url: format!("https://github.com/o/r/pull/{num}"),
            labels: s(labels),
            ready_vetted_at_head,
            producer_commented,
        }
    }

    #[test]
    fn lanes_doc_emits_every_state_with_the_right_members() {
        let prs = vec![
            qpr(1, &[], None, false),                     // un-vetted
            qpr(2, &["ai:ready"], Some(false), false),    // awaiting-re-vet
            qpr(3, &["ai:ready"], Some(true), false),     // ai:ready
            qpr(4, &["ai:reject"], None, false),          // ai:reject
            qpr(5, &["ai:relink"], None, false),          // ai:relink
            qpr(6, &["ai:design"], None, false),          // ai:design
            qpr(7, &["ai:close-candidate"], None, false), // ai:close-candidate (PR)
            qpr(8, &["ai:blocked-deploy"], None, false),  // producer-blocked
            qpr(9, &["ai:blocked-infra"], None, false),
            qpr(10, &["ai:blocked-on"], None, false),
            qpr(11, &["human:reject"], None, false), // human decisions
            qpr(12, &["human:design"], None, false),
            qpr(13, &["human:close-candidate"], None, false),
            qpr(14, &[], None, true),             // leak
            qpr(15, &["ai:reject"], None, false), // a second ai:reject member
        ];
        let doc = lanes_doc(&prs);

        // every state present, counts correct, membership disjoint (#15 joins #4 under ai:reject).
        let count = |lane: &str, st: &str| lane_state_count(&doc, lane, st);
        assert_eq!(count("vet-lifecycle", "un-vetted"), 1);
        assert_eq!(count("vet-lifecycle", "awaiting-re-vet"), 1);
        assert_eq!(count("vetter-verdicts", "ai:ready"), 1);
        assert_eq!(count("vetter-verdicts", "ai:reject"), 2);
        assert_eq!(count("vetter-verdicts", "ai:relink"), 1);
        assert_eq!(count("vetter-verdicts", "ai:design"), 1);
        assert_eq!(count("vetter-verdicts", "ai:close-candidate"), 1);
        assert_eq!(count("producer-blocked", "ai:blocked-deploy"), 1);
        assert_eq!(count("producer-blocked", "ai:blocked-infra"), 1);
        assert_eq!(count("producer-blocked", "ai:blocked-on"), 1);
        assert_eq!(count("human-decisions", "human:reject"), 1);
        assert_eq!(count("human-decisions", "human:design"), 1);
        assert_eq!(count("human-decisions", "human:close-candidate"), 1);
        assert_eq!(count("leak", "leak"), 1);

        // the PR list carries {repo, number, url, title}; the awaiting-re-vet member is #2.
        let arv = doc.pointer("/vet-lifecycle/awaiting-re-vet/prs/0").unwrap();
        assert_eq!(arv.get("number").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(arv.get("repo").and_then(|v| v.as_str()), Some("o/r"));
        assert_eq!(
            arv.get("url").and_then(|v| v.as_str()),
            Some("https://github.com/o/r/pull/2")
        );
        assert!(arv.get("title").is_some());

        // total across lanes == number of PRs (each bucketed exactly once).
        let mut total = 0usize;
        for (_, states) in doc.as_object().unwrap() {
            for (_, b) in states.as_object().unwrap() {
                total += b.get("count").and_then(|v| v.as_u64()).unwrap() as usize;
            }
        }
        assert_eq!(total, prs.len());
    }
}
