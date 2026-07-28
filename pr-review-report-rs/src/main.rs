// pr-review-report — report every open PR (and logged close-candidate) that needs a HUMAN decision,
// RESPECTING reviews already done: it reads verdict state from GitHub labels and
// and (b) GitHub's own review state (APPROVED / CHANGES_REQUESTED) on top of the CI/mergeability
// signal. Rust rewrite of pr-review-report.sh, fixing the 16 bugs from the adversarial review.
//
// Usage:   pr-review-report            # all buckets
//          pr-review-report --ready    # only the reviewed-&-ready-to-merge bucket
//          pr-review-report --queue [N]                 # cheapest-first review queue
//          pr-review-report --commit-closes <owner/repo> <pr>  # fail if a commit keyword closes an out-of-index issue
//          pr-review-report --deploy <owner/repo> <pr> [--network <net>] [--dry-run]  # sanctioned Zoltu deploy of a PR branch
// Config (env overrides cron.env in CWD, then default): ORG, ORGS (org scope for --queue), PR_ASSIGNEE.

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

/// One page of the reviewThreads GraphQL response → (unresolved count, end cursor if more pages).
/// None when the expected structure is missing (malformed/error response) — never a silent 0:
/// an unknown thread state must stay distinguishable from a verified-clean one.
fn count_unresolved_page(v: &Value) -> Option<(u64, Option<String>)> {
    let threads = v
        .get("data")?
        .get("repository")?
        .get("pullRequest")?
        .get("reviewThreads")?;
    let nodes = threads.get("nodes")?.as_array()?;
    let mut unresolved = 0u64;
    for n in nodes {
        // A node missing isResolved is malformed — treat the whole page as unknown.
        if !n.get("isResolved")?.as_bool()? {
            unresolved += 1;
        }
    }
    let page = threads.get("pageInfo")?;
    let cursor = if page.get("hasNextPage")?.as_bool()? {
        Some(page.get("endCursor")?.as_str()?.to_string())
    } else {
        None
    };
    Some((unresolved, cursor))
}

/// Hard cap on reviewThreads pages followed for ONE PR. At 100 threads a page that is 10,000
/// threads — far past any real PR — so hitting it means the cursor is not advancing (a server bug
/// or a hostile response). Stopping returns None, NOT the partial total: a truncated count read as
/// a total is exactly the "0 unresolved" lie this gate exists to prevent.
const MAX_THREAD_PAGES: usize = 100;

/// PURE given `fetch_page`: fold every reviewThreads page into one unresolved total. `fetch_page`
/// receives the cursor to resume from (`None` for the first page) and returns that page's raw JSON.
/// Paging stops at the first page whose `hasNextPage` is false. Returns None the moment ANY page is
/// unfetchable, unparseable, or the page cap is hit — a partial read is never reported as a total,
/// so a long review history can never silently truncate into a false clean.
fn total_unresolved(mut fetch_page: impl FnMut(Option<&str>) -> Option<Value>) -> Option<u64> {
    let mut total = 0u64;
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_THREAD_PAGES {
        let (page, next) = count_unresolved_page(&fetch_page(cursor.as_deref())?)?;
        total = total.checked_add(page)?;
        match next {
            Some(cur) => cursor = Some(cur),
            None => return Some(total),
        }
    }
    None
}

/// Total unresolved review threads on a PR (CodeRabbit or human), paginated so a long review
/// history is never silently truncated. None on any fetch/parse failure.
fn unresolved_threads(owner: &str, repo: &str, num: u64) -> Option<u64> {
    let query = "query($owner:String!,$repo:String!,$num:Int!,$cursor:String){\
                 repository(owner:$owner,name:$repo){pullRequest(number:$num){\
                 reviewThreads(first:100,after:$cursor){nodes{isResolved}\
                 pageInfo{hasNextPage endCursor}}}}}";
    total_unresolved(|cursor| {
        let mut args: Vec<String> = vec![
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-f".into(),
            format!("owner={owner}"),
            "-f".into(),
            format!("repo={repo}"),
            "-F".into(),
            format!("num={num}"),
        ];
        if let Some(cur) = cursor {
            args.push("-f".into());
            args.push(format!("cursor={cur}"));
        }
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        gh_json(&argrefs)
    })
}

/// Where an otherwise-presentable PR goes once its unresolved-thread count is known. THE THREE
/// OUTCOMES ARE DISTINCT ON PURPOSE: "verified clean", "verified dirty" and "could not tell" are
/// three different facts, and collapsing the third into either of the others is the bug this whole
/// gate exists to prevent (a green CodeRabbit status check while four threads sat unresolved on
/// rain-org-health#128 is the same class of mistake, made by trusting a proxy signal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ThreadRoute {
    /// A VERIFIED zero — the only state that reaches a human.
    Present,
    /// At least one unresolved thread — the producer's step-3e work.
    OpenThreads,
    /// The thread state could not be read. FAIL-CLOSED: not presented, and counted as a fetch
    /// error rather than silently as clean or as dirty, so a transient API failure is visible in
    /// the report instead of being laundered into either verdict.
    FetchError,
}

/// PURE: route a thread count. Only a verified zero passes (fail-closed, matching the queue's
/// existing mergeable check — an unverifiable PR is never presented to the human).
fn thread_route(threads: Option<u64>) -> ThreadRoute {
    match threads {
        Some(0) => ThreadRoute::Present,
        Some(_) => ThreadRoute::OpenThreads,
        None => ThreadRoute::FetchError,
    }
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

/// `owner/repo` from an ISSUE url. The twin of [`pr_slug`], which deliberately rejects `/issues/`
/// urls — reusing it here silently emptied the whole close-candidate queue, since every row parsed
/// to `None` and was skipped before it could be judged.
fn issue_slug(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    if !rest.contains("/issues/") {
        return None;
    }
    let slug = rest.split("/issues/").next()?;
    if slug.is_empty() || !slug.contains('/') {
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
    open_threads: usize, // otherwise-presentable but unresolved review threads — producer thread-resolution work
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
    let threads = if c.open_threads > 0 {
        format!(", {} open-threads", c.open_threads)
    } else {
        String::new()
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
        "review queue: {} ai:ready -> {} presentable, {} conflicting, {} red, {} pending, {} unknown-merge, {} approved, {} awaiting re-vet{}{}{} (cheapest first){}\n",
        c.raw, rows.len(), c.conflict, c.red, c.pending, c.merge_unknown, c.approved, c.unconfirmed, threads, err, excl, trunc
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
fn org_names(raw: &str) -> Vec<String> {
    let orgs: Vec<String> = raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if orgs.is_empty() {
        vec!["rainlanguage".to_string(), "cyclofinance".to_string()]
    } else {
        orgs
    }
}

fn parse_orgs(raw: &str) -> Vec<String> {
    org_names(raw)
        .into_iter()
        .flat_map(|o| ["--owner".to_string(), o])
        .collect()
}

/// The GraphQL `search` qualifier string for the org scope — same `ORGS` source and same
/// pure-fn/env-wrapper split as `parse_orgs` / `org_owner_args`, rendered as
/// `is:pr is:open org:<a> org:<b> …`. Both qualifiers matter: `search(type:ISSUE)` returns issues
/// as well as PRs, and dropping `is:open` would count closed PRs' references as live coverage.
fn org_search_query(raw: &str) -> String {
    let mut q = String::from("is:pr is:open");
    for o in org_names(raw) {
        q.push_str(" org:");
        q.push_str(&o);
    }
    q
}

fn org_owner_args() -> Vec<String> {
    parse_orgs(&std::env::var("ORGS").unwrap_or_default())
}

fn org_search_scope() -> String {
    org_search_query(&std::env::var("ORGS").unwrap_or_default())
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

    #[test]
    fn org_names_defaults_and_splits() {
        assert_eq!(super::org_names(""), ["rainlanguage", "cyclofinance"]);
        assert_eq!(super::org_names("a, b\tc"), ["a", "b", "c"]);
    }

    #[test]
    fn org_search_query_scopes_to_open_prs_in_every_org() {
        // The exact qualifier string, not "it contains an org" — `search(type:ISSUE)` also serves
        // ISSUES, and a closed PR's closing references are not live coverage, so losing either
        // `is:pr` or `is:open` silently changes what `uncovered-issues` counts as covered.
        assert_eq!(
            super::org_search_query(""),
            "is:pr is:open org:rainlanguage org:cyclofinance"
        );
        assert_eq!(
            super::org_search_query("S01-Issuer"),
            "is:pr is:open org:S01-Issuer"
        );
        // Every configured org is scoped, in order — the same split `org_names` does.
        assert_eq!(
            super::org_search_query("a, b\tc"),
            "is:pr is:open org:a org:b org:c"
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
        open_threads: 0,
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
                    // Open-threads gate: an otherwise-presentable PR with unresolved review
                    // threads (CodeRabbit or human) is the producer's thread-resolution work,
                    // not human-presentable. Only a VERIFIED zero passes (fail-closed): an
                    // unknown thread state counts as a fetch error, never a maybe-dirty row.
                    let Some((owner, repo)) = slug.split_once('/') else {
                        counts.fetch_error += 1;
                        continue;
                    };
                    match thread_route(unresolved_threads(owner, repo, *num)) {
                        ThreadRoute::Present => {
                            let (cost, basis) =
                                cost_from_comment(last_vetter_comment(&j).as_deref());
                            let repo_disp = slug.rsplit('/').next().unwrap_or(slug).to_string();
                            rows.push((cost, repo_disp, *num, url.clone(), basis));
                        }
                        ThreadRoute::OpenThreads => counts.open_threads += 1,
                        ThreadRoute::FetchError => counts.fetch_error += 1,
                    }
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
    // Wall-clock ms from the first tool RESULT to the first org-mutation's result. Its anchor is
    // the first `user` event, which is the result of the run's first tool call — so this window
    // opens one tool-result LATE and excludes that first call's own latency. That is a flaw, but
    // it is the meaning every committed record in metrics/runs.jsonl was written under, and
    // re-anchoring it would put a step in the series that no run ever experienced. `boot_ms` +
    // `ttl_ms` are the honest decomposition; this stays frozen. None when the run never mutated,
    // or when the anchor timestamps are absent/unparseable.
    startup_ms: Option<i64>,
    // LAUNCH overhead: first timestamped trace event → the run's first tool call. Nix resolving
    // and exec'ing the flake output, the MCP handshake, the prompt load — nothing model-driven.
    // Regresses when a derivation stops being cached or a store path is GC'd.
    boot_ms: Option<i64>,
    // ORIENTATION: the first tool call → the first PRODUCTIVE call (the producer's first mutation,
    // the vetter's first verdict). State-load, queue read, checkout, skill load. Entirely model-
    // and tool-surface-driven: it regresses when a tool returns too much, a prompt grows, or a
    // failed call makes the model improvise. Nothing it and `boot_ms` have in common is why one
    // fused number could not be acted on (#84).
    ttl_ms: Option<i64>,
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

/// The vetter's GitHub WRITES on the MCP tool surface.
///
/// The vetter has no Bash at all (#52): `record_verdict` and `record_close_candidate_verdict` are
/// its only two org mutations. A Bash-only detector therefore reports "never mutated" for every
/// modern vetter run — `firstMutationIndex` null, `startupMs` null, `startupPct` a meaningless
/// 100% — and takes the first-productive-act anchor down with it. These are the same transition
/// the old Bash `pr-review-report record-verdict` invocation was, just no longer expressible as a
/// command string.
const MCP_MUTATION_TOOLS: &[&str] = &[
    "mcp__fsm__record_verdict",
    "mcp__fsm__record_close_candidate_verdict",
];

/// A tool call is an org MUTATION when it is one of the vetter's MCP writes, or a Bash command
/// that creates/edits/merges/closes a PR or issue, or commits/pushes — i.e. the run stopped
/// recovering state and started doing work. Read-only gh/git/grep calls are NOT mutations.
fn is_mutation_tool(name: &str, input: &serde_json::Value) -> bool {
    if MCP_MUTATION_TOOLS.contains(&name) {
        return true;
    }
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

/// Which of the two startup numbers just became KNOWN.
///
/// The split exists because the numbers are emitted the moment their anchor arrives rather than at
/// run end. `run-metrics` only ever runs after the claude process exits, so a run that is killed,
/// times out, or dies leaves NO record at all — and that is precisely the run whose timings you
/// want. Three manual vetter runs on 2026-07-27/28 produced zero records between them (#84).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum StartupPhase {
    /// `bootMs` is known: the run reached its first tool call.
    Boot,
    /// `ttlMs` is known: the run reached its first productive call.
    Ttl,
}

impl StartupPhase {
    /// The wire word written into a partial record's `stage`.
    fn as_str(self) -> &'static str {
        match self {
            StartupPhase::Boot => "boot",
            StartupPhase::Ttl => "ttl",
        }
    }
}

/// The `stage` of a COMPLETE record — written at run end with the whole usage/cost/outcome tail.
///
/// How a consumer tells an old record from a new one: **`stage` absent means pre-#84**. Those
/// records carry `startupMs` under its original meaning and no `bootMs`/`ttlMs` at all, so a
/// chart spanning the change reads them as gaps in the new series rather than as zeroes, and the
/// `startupMs` series itself is continuous because that field's definition did not move.
const STAGE_FINAL: &str = "final";

/// Streaming detector for a run's startup anchors.
///
/// ONE state machine, fed events in order, driving BOTH the end-of-run `run-metrics` (over a whole
/// trace file) and the live `run-timings` filter (over the runner's pipe) — so the number a killed
/// run recorded mid-flight and the number a completed run records at the end can never be computed
/// two different ways.
#[derive(Default)]
struct StartupProbe {
    /// First timestamped event of ANY type: the earliest instant the trace can witness, and what
    /// `bootMs` measures from. Derived from the trace alone on purpose — a wall-clock anchor handed
    /// in by the runner could not be re-derived from an archived trace, and re-derivation from
    /// `runs/*.jsonl` is how this repo backfills history.
    run_ts: Option<i64>,
    /// First `user` timestamp — the result of the run's FIRST tool call. This is `startupMs`'s
    /// anchor and is deliberately left where it is; see the field's comment on [`RunMetrics`].
    first_result_ts: Option<i64>,
    /// Timestamp of the assistant event carrying the run's first `tool_use`.
    first_tool_ts: Option<i64>,
    /// Set once the first tool call is seen, whether or not its event was timestamped, so an
    /// untimestamped first call degrades to "no boot/ttl" instead of promoting a LATER call into
    /// the anchor and reporting a plausible, wrong number.
    saw_first_tool: bool,
    /// Timestamp of the assistant event carrying the first PRODUCTIVE `tool_use`. Anchored on the
    /// call, not on its result: the model has finished orienting when it issues the call, and a run
    /// killed in the gap before the result would otherwise lose the whole measurement.
    productive_ts: Option<i64>,
    tool_calls: usize,
    startup_tool_calls: usize,
    wakeup_calls: usize,
    first_mutation_index: Option<usize>,
    mutation_pending: bool,
    mutation_result_ts: Option<i64>,
}

impl StartupProbe {
    /// Feed one trace event. Returns the phases whose number became known ON THIS EVENT — at most
    /// two, and two at once when the run's very first tool call is already the productive one.
    fn observe(&mut self, ev: &Value) -> Vec<StartupPhase> {
        let mut phases = Vec::new();
        let ts = ev
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_epoch_ms);
        if ts.is_some() && self.run_ts.is_none() {
            self.run_ts = ts;
        }
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let Some(content) = ev
                    .get("message")
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_array())
                else {
                    return phases;
                };
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                        continue;
                    }
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let empty = serde_json::json!({});
                    let input = block.get("input").unwrap_or(&empty);
                    if name == "ScheduleWakeup" || name == "CronCreate" {
                        self.wakeup_calls += 1;
                    }
                    if !self.saw_first_tool {
                        self.saw_first_tool = true;
                        self.first_tool_ts = ts;
                        if self.boot_ms().is_some() {
                            phases.push(StartupPhase::Boot);
                        }
                    }
                    if self.first_mutation_index.is_none() {
                        if is_mutation_tool(name, input) {
                            self.first_mutation_index = Some(self.tool_calls);
                            self.mutation_pending = true;
                            self.productive_ts = ts;
                            if self.ttl_ms().is_some() {
                                phases.push(StartupPhase::Ttl);
                            }
                        } else {
                            self.startup_tool_calls += 1;
                        }
                    }
                    self.tool_calls += 1;
                }
            }
            Some("user") => {
                // A tool RESULT. The first one anchors `startupMs`; once a mutation is pending,
                // the next one closes that window.
                if let Some(ts) = ts {
                    if self.first_result_ts.is_none() {
                        self.first_result_ts = Some(ts);
                    }
                    if self.mutation_pending {
                        self.mutation_result_ts = Some(ts);
                        self.mutation_pending = false;
                    }
                }
            }
            _ => {}
        }
        phases
    }

    fn boot_ms(&self) -> Option<i64> {
        Some(self.first_tool_ts? - self.run_ts?)
    }

    fn ttl_ms(&self) -> Option<i64> {
        Some(self.productive_ts? - self.first_tool_ts?)
    }

    fn startup_ms(&self) -> Option<i64> {
        Some(self.mutation_result_ts? - self.first_result_ts?)
    }

    /// Copy the counted + timed fields onto a metrics record.
    fn fill(&self, m: &mut RunMetrics) {
        m.tool_calls = self.tool_calls;
        m.startup_tool_calls = self.startup_tool_calls;
        m.wakeup_calls = self.wakeup_calls;
        m.first_mutation_index = self.first_mutation_index;
        m.startup_ms = self.startup_ms();
        m.boot_ms = self.boot_ms();
        m.ttl_ms = self.ttl_ms();
    }
}

/// Parse a stream-json trace: count tool calls in order, find the first mutation, and take
/// the usage/duration/cost from the result event with the most turns (the main run — trailing
/// short result events from continuations are ignored).
fn run_metrics(content: &str) -> RunMetrics {
    let mut m = RunMetrics::default();
    let mut probe = StartupProbe::default();
    let mut best_turns = 0u64;
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        probe.observe(&v);
        if v.get("type").and_then(|t| t.as_str()) == Some("result") {
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
    }
    probe.fill(&mut m);
    m
}

/// Which run a record belongs to, as the runner knows it. Nothing here is derivable from the
/// trace, which is why it arrives as flags.
struct RunIdentity<'a> {
    run_id: Option<&'a str>,
    role: Option<&'a str>,
    model: Option<&'a str>,
}

/// Stamp a record with the run's identity.
///
/// Only the fields the caller actually supplied are added, so a bare `run-metrics <trace>` keeps
/// emitting exactly the record it always has (the dashboard re-derives history from raw traces).
fn stamp_identity(doc: &mut Value, id: &RunIdentity) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    if let Some(run_id) = id.run_id {
        obj.insert("runId".into(), serde_json::json!(run_id));
    }
    if let Some(role) = id.role {
        obj.insert("role".into(), serde_json::json!(role));
    }
    if let Some(model) = id.model {
        obj.insert("model".into(), serde_json::json!(model));
    }
}

/// One PARTIAL record: everything known at the instant `phase` became knowable, and nothing else.
///
/// The omissions are the point. A `boot` record has no `ttlMs` because the run has not reached its
/// first productive call; neither stage has `toolCalls`, `startupPct`, `durationMs` or `outcome`,
/// because the run is still going and any value for them would be a guess that later reads as
/// measurement. `startupMs` is absent from both because its end anchor is the mutation's RESULT,
/// which has not arrived when the `ttl` record is written. A consumer distinguishes these from a
/// complete record by `stage`, and reconciles the lines one `runId` can produce by keeping the
/// most complete (`final` > `ttl` > `boot`) and, among equals, the LAST — model fallback re-runs
/// this filter per attempt, and the last attempt is the one that actually ran.
fn partial_record(
    probe: &StartupProbe,
    phase: StartupPhase,
    trace: &str,
    id: &RunIdentity,
) -> Value {
    let mut doc = match phase {
        StartupPhase::Boot => serde_json::json!({
            "trace": trace,
            "stage": phase.as_str(),
            "bootMs": probe.boot_ms(),
        }),
        StartupPhase::Ttl => serde_json::json!({
            "trace": trace,
            "stage": phase.as_str(),
            "startupToolCalls": probe.startup_tool_calls,
            "firstMutationIndex": probe.first_mutation_index,
            "bootMs": probe.boot_ms(),
            "ttlMs": probe.ttl_ms(),
        }),
    };
    stamp_identity(&mut doc, id);
    doc
}

/// Append one record to the metrics file, creating it (and its directory) if absent.
///
/// Best-effort by contract: this runs INSIDE the runner's live pipe, and a metrics write must
/// never be able to take down the run it is measuring. One `write` of a short line to an O_APPEND
/// fd is atomic on Linux, so these mid-run appends cannot interleave with the runner's end-of-run
/// `run-metrics >> …` into the same file.
fn append_record(path: &str, doc: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let dir = std::path::Path::new(path).parent();
    if let Some(dir) = dir {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(doc).unwrap())?;
    f.flush()
}

/// `run-timings --out <runs.jsonl> --trace <path> [--run-id --role --model]`: pass a LIVE
/// stream-json trace through unchanged, appending a record the moment `bootMs` and then `ttlMs`
/// become known.
///
/// This exists because `run-metrics` writes at the END of a run. A run that is killed, times out,
/// or dies therefore records nothing — and that is exactly the run whose startup timings matter
/// (#84: three manual vetter runs produced zero records between them, so the 5-minute time-to-
/// first-verdict had to be measured off the trace by hand). Sitting in the runner's pipe, this
/// filter has both numbers on disk long before the run's fate is decided.
///
/// It is a pass-through FIRST: every line is forwarded before it is parsed, so the downstream
/// distiller keeps seeing the whole stream even if nothing here works. Every failure — an
/// unparseable line, an unwritable metrics file — is swallowed for the same reason.
fn run_timings_mode(out: &str, trace: &str, id: &RunIdentity) -> i32 {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut probe = StartupProbe::default();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if writeln!(stdout, "{line}").is_err() {
            return 0; // downstream closed
        }
        let _ = stdout.flush();
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for phase in probe.observe(&ev) {
            let _ = append_record(out, &partial_record(&probe, phase, trace, id));
        }
    }
    0
}

/// `run-metrics <trace.jsonl> [--run-id --role --model --exit-code]`: print the run's metrics
/// (startup overhead, duration, tokens, cost) as one JSON line — the input to a committed
/// metrics/runs.jsonl and the #7 dashboard.
///
/// With the run-identity flags it emits the FULL runs.jsonl record. The runners used to pipe this
/// through `jq '. + {runId:…, role:…, model:…, exitCode:…, outcome:…}'`; folding the merge in here
/// means the record's shape lives in one tested place, and `outcome` is derived by
/// [`classify_trace`] rather than by grepping the trace in bash.
fn run_metrics_mode(
    path: &str,
    id: &RunIdentity,
    exit_code: Option<i32>,
    preflight_missing: &[String],
) -> i32 {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read trace {path}: {e}");
            return 2;
        }
    };
    let m = run_metrics(&content);
    let tooling = trace_tooling_report(&content);
    let outcome = exit_code.map(|rc| (rc, classify_outcome(&content, rc, preflight_missing)));
    println!(
        "{}",
        serde_json::to_string(&final_record(
            path,
            &m,
            id,
            outcome,
            &tooling,
            preflight_missing
        ))
        .unwrap()
    );
    0
}

/// The COMPLETE end-of-run record: every field in `partial_record` plus the counts, usage, cost and
/// outcome that only exist once the run has finished. Built here rather than inline in
/// [`run_metrics_mode`] so the record's shape — `stage` above all — is a tested value, not a
/// side effect of printing.
fn final_record(
    path: &str,
    m: &RunMetrics,
    id: &RunIdentity,
    outcome: Option<(i32, TraceOutcome)>,
    tooling: &ToolingReport,
    preflight_missing: &[String],
) -> Value {
    let mut doc = serde_json::json!({
        "trace": path,
        "stage": STAGE_FINAL,
        "toolCalls": m.tool_calls,
        "startupToolCalls": m.startup_tool_calls,
        "startupPct": (m.startup_pct() * 10.0).round() / 10.0,
        "wakeupCalls": m.wakeup_calls,
        "firstMutationIndex": m.first_mutation_index,
        "bootMs": m.boot_ms,
        "ttlMs": m.ttl_ms,
        "startupMs": m.startup_ms,
        "durationMs": m.duration_ms,
        "numTurns": m.num_turns,
        "tokensIn": m.tokens_in,
        "tokensOut": m.tokens_out,
        "cacheRead": m.cache_read,
        "cacheCreation": m.cache_creation,
        "costUsd": (m.cost_usd * 1000.0).round() / 1000.0,
        // Always present, so the dashboard can distinguish "no tooling trouble" from "this record
        // predates the field". `unreadableFiles` is what makes the outcome `tooling-failure`;
        // `commandsNotFound` is reported but never decides the outcome (see trace_tooling_report).
        "unreadableFiles": tooling.unreadable,
        "commandsNotFound": tooling.command_not_found,
        "missingTools": preflight_missing,
    });
    stamp_identity(&mut doc, id);
    if let (Some(obj), Some((rc, verdict))) = (doc.as_object_mut(), outcome) {
        obj.insert("exitCode".into(), serde_json::json!(rc));
        obj.insert("outcome".into(), serde_json::json!(verdict.as_str()));
    }
    doc
}

// ---------------------------------------------------------------------------------------------
// Tooling failures (#85).
//
// A run whose TOOLS could not do their job is not a successful run. Before this, one could be:
// the vetter's `Read` of `audit/protofire/*.pdf` came back `isError: pdftoppm is not installed`,
// the model recorded a verdict on what it could still check, claude exited 0, and the run was
// classified `ok`. The same PR was vetted `reject` by a run that read more of it and `ready` by
// the run that could not — the missing dependency did not merely reduce coverage, it moved the
// verdict.
//
// Two mechanisms, because the failure has two shapes:
//
//   KNOWN — a dependency we have already discovered. [`HARNESS_TOOLS`] declares it, the runner
//   resolves it BEFORE spending a token, and a missing one ends the run then and there. No model
//   runs, so no confident answer is built on evidence it cannot read.
//
//   UNKNOWN — a dependency nobody has hit yet, which by definition cannot be pre-declared. Caught
//   from the trace by [`trace_tooling_report`], on a structural relation rather than on the error
//   prose: a file the environment ITSELF listed as existing, which the run then could not read.
//   That is an environment failure whatever the artifact type and whatever the message says, so
//   the NEXT undeclared reader dependency fails the same way this one does, with no new code.
// ---------------------------------------------------------------------------------------------

/// An external binary the HARNESS execs on the model's behalf, with the evidence for needing it.
///
/// Nothing in this repo — no script, no prompt, no skill — names these. They are reached at READ
/// time by the harness's own `Read` tool, which is exactly why the #77 closure gate could not see
/// them: that gate proves every DECLARED tool resolves from the locked nixpkgs, and an undeclared
/// one is invisible to a walk over declarations. This table is where such a dependency becomes
/// declared, and `.github/workflows/rust.yml` asserts each entry is in both model runners' closures.
struct HarnessTool {
    /// The executable name, as the harness spawns it.
    bin: &'static str,
    /// Why the pipeline needs it — a fact that can be checked, not a guess.
    why: &'static str,
}

/// The declared set. Deliberately short: a preflight that demands tools the work does not use gets
/// switched off the first time it blocks a run for nothing.
///
/// Derived by enumerating the literal command names the harness's own exec helper can be called
/// with (`strings claude | grep -o 'an("[a-z…]*"'` — its spawn helper is
/// `function an(e,t,r={timeout:…,useCwd:…})`), then keeping only those reachable from a tool the
/// pipeline's models actually hold, on Linux, for an artifact type the audited repos contain.
/// Everything else in that enumeration is platform glue (`osascript`, `powershell.exe`, `pacman`,
/// `tmux`, `xclip`, …), plugin-install machinery the crons disable via `--strict-mcp-config`
/// (`unzip`, which the harness also falls back to a JS implementation for), or already declared
/// (`gh`, `git`).
const HARNESS_TOOLS: &[HarnessTool] = &[
    HarnessTool {
        bin: "pdftoppm",
        why: "Read renders PDF pages with `pdftoppm -jpeg -r 100`; the org's external audit \
              evidence is audit/protofire/*.pdf (110 files across the work clones)",
    },
    HarnessTool {
        bin: "pdfinfo",
        why: "Read asks pdfinfo for a PDF's page count before rendering a requested page range",
    },
];

/// Resolve `bin` against `$PATH`, returning the first executable match.
///
/// The structural half of the discriminant: "a dependency is absent" is a fact about the
/// ENVIRONMENT, decidable with a directory walk and an executable-bit test, with no error text to
/// interpret. It cannot be confused with "the input was bad" because it never looks at an input.
/// The PATH string is a PARAMETER so the lookup is testable without mutating the process
/// environment — a test that sets `PATH` would race every other test in the binary.
fn resolve_in(path: &std::ffi::OsStr, bin: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in std::env::split_paths(path) {
        // An empty PATH element means "the current directory" to some shells. Skipped here: a
        // runner must not acquire a tool from whatever directory the cron happened to start in.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(bin);
        let Ok(md) = std::fs::metadata(&cand) else {
            continue;
        };
        if md.is_file() && md.permissions().mode() & 0o111 != 0 {
            return Some(cand);
        }
    }
    None
}

/// `preflight`: resolve every [`HARNESS_TOOLS`] entry against the process's own `$PATH`, print
/// what was found, exit non-zero when any is missing.
///
/// Exit [`CLOSURE_UNSATISFIED`] is its own code (like the usage gate's 10) so the runner can tell
/// "a dependency is missing" from "the binary itself failed".
///
/// The environment is a PARAMETER of [`preflight_report`], not a global read, which is what lets
/// the CI closure gate ask the identical question of a runner's BAKED path — one implementation,
/// so the declaration and the closure cannot answer differently.
fn preflight_mode() -> i32 {
    preflight_report(&std::env::var_os("PATH").unwrap_or_default())
}

fn preflight_report(path: &std::ffi::OsStr) -> i32 {
    let mut missing = Vec::new();
    for t in HARNESS_TOOLS {
        match resolve_in(path, t.bin) {
            Some(p) => println!("ok      {:<10} {}", t.bin, p.display()),
            None => {
                println!("MISSING {:<10} {}", t.bin, t.why);
                missing.push(t.bin);
            }
        }
    }
    if missing.is_empty() {
        return 0;
    }
    // stdout carries the machine-readable list; the runner passes it straight to `run-metrics`.
    println!("missing={}", missing.join(","));
    eprintln!(
        "error: {} harness tool(s) missing from PATH: {}",
        missing.len(),
        missing.join(", ")
    );
    CLOSURE_UNSATISFIED
}

// ---------------------------------------------------------------------------------------------
// The closure gates (#85).
//
// [`HARNESS_TOOLS`] is a DECLARATION. These three subcommands are what make it true of the thing
// a model actually runs inside: the nix closure baked into each model runner. CI calls each in one
// line and reads the exit code.
//
// They share one rule, and it is the whole reason they exist separately from the runners' own
// `preflight`: a runner's capabilities are resolved from ITS OWN baked PATH and from nothing else.
// A tool that merely happens to be installed on the CI box must never satisfy a gate, because the
// cron's environment is bare and the developer's is not.
//
// Why three, and not one:
//
//   `closure-preflight` — presence. Every declared read-time dependency resolves from each
//   runner's closure. Catches "nobody added it to runtimeInputs".
//
//   `closure-render`    — capability. Presence is not function: a closure can carry `pdftoppm`
//   and still fail on a missing shared library or data file, and the harness turns BOTH into the
//   same `isError` with the run still exiting 0. So this one renders a real PDF end to end with
//   the harness's own argv.
//
//   `closure-surface`   — divergence. The producer and the vetter run the same harness over the
//   same repositories, so a capability one needs is almost always one the other needs. Note this
//   gate is structurally blind to a capability BOTH runners lack — which is exactly #85's shape.
//   That is why the two presence gates above are what actually hold the bug, and why a symmetry
//   check alone would have been a false comfort.
// ---------------------------------------------------------------------------------------------

/// Exit code for "the closure does not satisfy the gate" — the same code the runners' own
/// `preflight` exits with, because it is the same fact about the same declaration.
const CLOSURE_UNSATISFIED: i32 = 12;

/// Exit code for "the gate could not be evaluated": no package was built, or the built wrapper
/// had no PATH in it. Distinct from [`CLOSURE_UNSATISFIED`] because knowing NOTHING about a
/// closure is a different problem from knowing it is wrong, and sends the reader somewhere else.
const CLOSURE_UNEVALUABLE: i32 = 2;

/// The packages whose closure IS a model's environment.
///
/// Only these two host a harness. The other flake outputs (`refresh-human-queue`, the human entry
/// points) run no model, read no PDF, and are deliberately not held to the harness declaration.
const MODEL_RUNNERS: &[&str] = &["campaign-run", "review-run"];

/// A binary one model runner has and the other does not, ON PURPOSE.
struct DeclaredAsymmetry {
    bin: &'static str,
    /// The model runner that has it. Every other model runner must NOT.
    only_in: &'static str,
    /// Why it is one-sided — a fact about the role, not a shrug.
    why: &'static str,
}

/// Every sanctioned difference between the model runners' capability surfaces.
///
/// #85 is precisely the shape of bug where a tool lands in one closure and not the other. Rather
/// than trusting that whoever edits `runtimeInputs` remembers both, the asymmetry is declared, and
/// `closure-surface` fails on any difference that is not in this table — and equally on any entry
/// here that is no longer true, so the declaration cannot outlive the fact and quietly license a
/// real divergence later.
const DECLARED_ASYMMETRY: &[DeclaredAsymmetry] = &[DeclaredAsymmetry {
    bin: "jq",
    only_in: "campaign-run",
    why: "the PRODUCER's model shells out to jq from its Bash tool; the vetter has no Bash and \
          cannot invoke anything at all (flake.nix)",
}];

/// Why a closure gate is not satisfied.
///
/// A TYPE, so the exit code and the diagnostic are derived from the fact. Nothing downstream
/// re-reads a rendered message to work out what happened.
#[derive(Debug, PartialEq, Eq)]
enum ClosureFault {
    /// `nix build` produced no store path for the package.
    Unbuildable { pkg: String, detail: String },
    /// The built wrapper carries no `export PATH="…"`, or an empty one. `writeShellApplication`'s
    /// shape changed; a gate that read that as "no tools missing" would be worse than no gate.
    NoBakedPath { pkg: String, script: String },
    /// A declared harness tool does not resolve from this runner's own baked PATH.
    ToolMissing {
        pkg: String,
        bin: &'static str,
        why: &'static str,
    },
    /// The render did not happen. What `command -v` cannot see.
    RenderFailed { pkg: String, how: RenderFault },
    /// A binary present to one runner and absent from the other, with nothing declaring why.
    UndeclaredAsymmetry { pkg: String, bin: String },
    /// A [`DECLARED_ASYMMETRY`] entry that is no longer true.
    StaleAsymmetry {
        pkg: &'static str,
        bin: &'static str,
    },
    /// A runner with no store binaries at all. Two empty surfaces compare equal, so without this
    /// the symmetry gate would pass loudest exactly when a closure was most broken.
    EmptySurface { pkg: String },
}

impl ClosureFault {
    /// Whether the gate learned nothing, as opposed to learning something bad.
    fn is_unevaluable(&self) -> bool {
        matches!(
            self,
            ClosureFault::Unbuildable { .. } | ClosureFault::NoBakedPath { .. }
        )
    }

    fn describe(&self) -> String {
        match self {
            ClosureFault::Unbuildable { pkg, detail } => {
                format!("{pkg}: nix build produced no package ({detail})")
            }
            ClosureFault::NoBakedPath { pkg, script } => format!(
                "{pkg}: no `export PATH=\"…\"` in {script} — writeShellApplication's shape changed, \
                 so this gate cannot see the closure at all"
            ),
            ClosureFault::ToolMissing { pkg, bin, why } => format!(
                "{pkg}: `{bin}` is not in the closure. Add it to this runner's runtimeInputs — {why}"
            ),
            ClosureFault::RenderFailed { pkg, how } => format!(
                "{pkg}: the PDF read path does not render — {}. The harness turns this into the \
                 same `isError` a MISSING binary produces, and the run still exits 0.",
                how.describe()
            ),
            ClosureFault::UndeclaredAsymmetry { pkg, bin } => format!(
                "{pkg}: `{bin}` is in this runner's closure and not the other's. Add it to BOTH \
                 runners' runtimeInputs, or add it to DECLARED_ASYMMETRY with the reason it is \
                 one-sided."
            ),
            ClosureFault::StaleAsymmetry { pkg, bin } => format!(
                "DECLARED_ASYMMETRY says `{bin}` is only in {pkg}, and it is not. Drop the \
                 declaration — a stale one licenses a real divergence later."
            ),
            ClosureFault::EmptySurface { pkg } => format!(
                "{pkg}: not one /nix/store binary on its baked PATH. Two empty surfaces compare \
                 equal, so this gate must not call that symmetric."
            ),
        }
    }
}

/// The process exit code for a set of faults.
///
/// An un-evaluable gate OUTRANKS a failed one. If `nix build` did not produce a package there is
/// no closure to have an opinion about, and reporting "this closure is missing a tool" for what is
/// really a broken build sends whoever reads it to the wrong file.
fn closure_exit_code(faults: &[ClosureFault]) -> i32 {
    if faults.is_empty() {
        return 0;
    }
    if faults.iter().any(ClosureFault::is_unevaluable) {
        return CLOSURE_UNEVALUABLE;
    }
    CLOSURE_UNSATISFIED
}

/// Print every fault as a CI annotation and return the exit code, or print `ok_line` and return 0.
fn report_closure(faults: &[ClosureFault], ok_line: &str) -> i32 {
    for f in faults {
        println!("::error::{}", f.describe());
    }
    if faults.is_empty() {
        println!("{ok_line}");
    }
    closure_exit_code(faults)
}

/// A model runner's environment, exactly as the runner will see it.
struct RunnerClosure {
    pkg: String,
    /// The PATH `writeShellApplication` baked into the script from `runtimeInputs`.
    path: std::ffi::OsString,
}

/// Lift `export PATH="…"` out of a built runner script.
///
/// Pure, because the interesting failure is the SHAPE of the script rather than the filesystem:
/// nixpkgs could stop emitting this line, and a gate that then reported an empty environment as
/// "nothing missing" would be silently vacuous. An empty value is treated as absent for the same
/// reason.
fn baked_path(script: &str) -> Option<&str> {
    let (_, rest) = script.split_once("export PATH=\"")?;
    let (path, _) = rest.split_once('"')?;
    if path.is_empty() {
        return None;
    }
    Some(path)
}

/// Build one model runner and read the PATH out of it.
fn runner_closure(flake: &str, pkg: &str) -> Result<RunnerClosure, ClosureFault> {
    let out = Command::new("nix")
        .arg("build")
        .arg("--no-link")
        .arg("--print-out-paths")
        .arg(format!("{flake}#{pkg}"))
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            return Err(ClosureFault::Unbuildable {
                pkg: pkg.to_string(),
                detail: format!("cannot run nix: {e}"),
            })
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(ClosureFault::Unbuildable {
            pkg: pkg.to_string(),
            detail: err.trim().lines().last().unwrap_or("no output").to_string(),
        });
    }
    let store = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    // The package name IS the script name for every model runner (flake.nix's `runner` builder
    // takes one `name`), so there is no second place that has to agree about the binary's name.
    let script = format!("{store}/bin/{pkg}");
    let text = std::fs::read_to_string(&script).unwrap_or_default();
    match baked_path(&text) {
        Some(p) => Ok(RunnerClosure {
            pkg: pkg.to_string(),
            path: std::ffi::OsString::from(p),
        }),
        None => Err(ClosureFault::NoBakedPath {
            pkg: pkg.to_string(),
            script,
        }),
    }
}

/// Every model runner's closure, or the fault that stopped each from being read.
fn model_runner_closures(flake: &str) -> (Vec<RunnerClosure>, Vec<ClosureFault>) {
    let mut ok = Vec::new();
    let mut faults = Vec::new();
    for pkg in MODEL_RUNNERS {
        match runner_closure(flake, pkg) {
            Ok(c) => ok.push(c),
            Err(f) => faults.push(f),
        }
    }
    (ok, faults)
}

/// `closure-preflight`: every [`HARNESS_TOOLS`] entry resolves from EACH model runner's own baked
/// PATH.
///
/// The sibling "each runner's closure is self-contained" gate asserts the tools the SCRIPTS name.
/// It structurally cannot see a tool only the harness reaches for at read time — nothing in this
/// repo names `pdftoppm`, so no walk over declarations could find it, which is how #85 passed a
/// gate that was working correctly. This is the other half: the declaration is `HARNESS_TOOLS`,
/// and it is enforced against the closure by the same `resolve_in` the runner uses at run time.
fn closure_preflight_mode(flake: &str) -> i32 {
    let (closures, mut faults) = model_runner_closures(flake);
    for c in &closures {
        for t in HARNESS_TOOLS {
            match resolve_in(&c.path, t.bin) {
                Some(p) => println!("  {:<13} {:<10} {}", c.pkg, t.bin, p.display()),
                None => faults.push(ClosureFault::ToolMissing {
                    pkg: c.pkg.clone(),
                    bin: t.bin,
                    why: t.why,
                }),
            }
        }
    }
    report_closure(
        &faults,
        "every harness read-time dependency is in both model runners' closures",
    )
}

/// How a render failed, in the harness's own terms.
#[derive(Debug, PartialEq, Eq)]
enum RenderFault {
    /// `pdftoppm` is not on the closure's PATH at all.
    NotInClosure,
    /// The process could not be started: a broken loader, a non-executable file, a dangling
    /// symlink out of the store.
    Unspawnable(std::io::ErrorKind),
    /// It ran and exited non-zero. THIS is the case presence cannot distinguish from absence — a
    /// missing shared library or data file lands here and reaches the model as the same `isError`.
    Exited { code: Option<i32>, stderr: String },
    /// Exit 0 with nothing written. The harness counts the files `pdftoppm` left behind, so zero
    /// files is its "could not read this document" path.
    NoPages,
    /// A page was written that is not a JPEG, so the harness has nothing it can attach.
    NotJpeg { page: String, magic: Vec<u8> },
}

impl RenderFault {
    fn describe(&self) -> String {
        match self {
            RenderFault::NotInClosure => "`pdftoppm` is not on the runner's baked PATH".to_string(),
            RenderFault::Unspawnable(kind) => format!("`pdftoppm` would not start ({kind:?})"),
            RenderFault::Exited { code, stderr } => format!(
                "`pdftoppm` exited {}: {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "on a signal".to_string()),
                if stderr.is_empty() {
                    "(no stderr)"
                } else {
                    stderr
                }
            ),
            RenderFault::NoPages => "`pdftoppm` exited 0 and wrote no pages".to_string(),
            RenderFault::NotJpeg { page, magic } => {
                format!("{page} is not a JPEG (magic {magic:02x?})")
            }
        }
    }
}

/// The three bytes every JPEG starts with (SOI + the first marker). What the harness attaches has
/// to be a real image, not a zero-length file `pdftoppm` opened and abandoned.
const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];

/// The one-page PDF the render gate feeds to poppler.
///
/// GENERATED, not committed, on purpose: a built fixture cannot rot into a stale blob nobody knows
/// how to regenerate, and the byte offsets are its self-check — poppler reports a broken xref
/// loudly, so a generator that computes them wrong fails the gate instead of passing a degenerate
/// file through it.
///
/// It is a real document rather than a header: catalog → pages → page → content stream → font,
/// with a MediaBox and a text-showing operator, so a render has something to rasterise and the
/// parse has an object graph to walk.
fn fixture_pdf() -> Vec<u8> {
    let stream = "BT /F1 36 Tf 72 700 Td (POPPLER RENDER OK) Tj ET\n";
    let objects = [
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_string(),
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n".to_string(),
        format!("4 0 obj\n<< /Length {} >>\nstream\n{stream}endstream\nendobj\n", stream.len()),
        "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n".to_string(),
    ];

    let mut pdf = Vec::from(&b"%PDF-1.4\n"[..]);
    let mut offsets = Vec::new();
    for o in &objects {
        // Taken BEFORE the object is written: an xref entry is the offset OF the object.
        offsets.push(pdf.len());
        pdf.extend_from_slice(o.as_bytes());
    }
    let startxref = pdf.len();
    pdf.extend_from_slice(b"xref\n");
    // Entry 0 is the head of the free list, and the count includes it.
    pdf.extend_from_slice(format!("0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// Render `pdf` with a runner's own `pdftoppm`, and return how many pages came out.
///
/// The argv is byte-for-byte the harness's: `Read` renders a PDF page range with
/// `pdftoppm -jpeg -r 100 [-f N] [-l M] <pdf> <prefix>`. Approximating it would test a call the
/// pipeline never makes.
///
/// `env_clear` is the `env -i` of the gate this replaces, and stronger: not one variable of the CI
/// box's environment reaches the renderer, so nothing installed there can stand in for the
/// closure's own copy. `HOME` is set into the scratch dir because poppler reads user config.
fn render_from_closure(
    closure: &RunnerClosure,
    pdf: &std::path::Path,
    dir: &std::path::Path,
) -> Result<usize, RenderFault> {
    let Some(bin) = resolve_in(&closure.path, "pdftoppm") else {
        return Err(RenderFault::NotInClosure);
    };
    let out = Command::new(&bin)
        .args(["-jpeg", "-r", "100", "-f", "1", "-l", "1"])
        .arg(pdf)
        .arg(dir.join("page"))
        .env_clear()
        .env("PATH", &closure.path)
        .env("HOME", dir)
        .output()
        .map_err(|e| RenderFault::Unspawnable(e.kind()))?;
    if !out.status.success() {
        return Err(RenderFault::Exited {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let mut pages: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| RenderFault::Unspawnable(e.kind()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jpg"))
        .collect();
    pages.sort();
    let Some(first) = pages.first() else {
        return Err(RenderFault::NoPages);
    };
    let bytes = std::fs::read(first).map_err(|e| RenderFault::Unspawnable(e.kind()))?;
    if !bytes.starts_with(&JPEG_MAGIC) {
        return Err(RenderFault::NotJpeg {
            page: first.display().to_string(),
            magic: bytes.iter().take(JPEG_MAGIC.len()).copied().collect(),
        });
    }
    Ok(pages.len())
}

/// `closure-render`: each model runner's closure RENDERS a PDF, not merely resolves the renderer.
///
/// `command -v pdftoppm` proves a name resolves. It does not prove the renderer runs, and the
/// difference is invisible from outside: the harness turns a non-zero `pdftoppm` into the same
/// `isError` an absent one produces, and the run still exits 0. So this executes the real thing on
/// a real document, from the runner's own closure.
fn closure_render_mode(flake: &str) -> i32 {
    let (closures, mut faults) = model_runner_closures(flake);
    let root = std::env::temp_dir().join(format!("prr-closure-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("error: cannot create {}: {e}", root.display());
        return CLOSURE_UNEVALUABLE;
    }
    let pdf = root.join("fixture.pdf");
    if let Err(e) = std::fs::write(&pdf, fixture_pdf()) {
        eprintln!("error: cannot write {}: {e}", pdf.display());
        let _ = std::fs::remove_dir_all(&root);
        return CLOSURE_UNEVALUABLE;
    }
    for c in &closures {
        let dir = root.join(&c.pkg);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("error: cannot create {}: {e}", dir.display());
            let _ = std::fs::remove_dir_all(&root);
            return CLOSURE_UNEVALUABLE;
        }
        match render_from_closure(c, &pdf, &dir) {
            Ok(n) => println!("  {:<13} rendered {n} page(s), JPEG magic OK", c.pkg),
            Err(how) => faults.push(ClosureFault::RenderFailed {
                pkg: c.pkg.clone(),
                how,
            }),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    report_closure(
        &faults,
        "both model runners can render a PDF from their own closure",
    )
}

/// Where nix keeps the closures. Only directories under here count towards a capability surface.
const NIX_STORE: &str = "/nix/store";

/// Every binary a runner can invoke: the names in every directory on `path` that nix owns.
///
/// Store directories ONLY. A non-store entry on a runner's PATH is the ambient-acquisition bug the
/// sibling "no ambient tool acquisition" gate already forbids, and counting it here would let a
/// tool the developer happens to have installed silence a real asymmetry.
///
/// The store root is a PARAMETER for the same reason `resolve_in`'s PATH is: the rule is what
/// needs testing, and no test can write into `/nix/store`.
fn capability_surface_under(
    path: &std::ffi::OsStr,
    store: &std::path::Path,
) -> std::collections::BTreeSet<String> {
    let mut bins = std::collections::BTreeSet::new();
    for dir in std::env::split_paths(path) {
        if !dir.starts_with(store) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if let Some(n) = e.file_name().to_str() {
                bins.insert(n.to_string());
            }
        }
    }
    bins
}

fn capability_surface(closure: &RunnerClosure) -> std::collections::BTreeSet<String> {
    capability_surface_under(&closure.path, std::path::Path::new(NIX_STORE))
}

/// The faults in a set of capability surfaces, given [`DECLARED_ASYMMETRY`].
///
/// PURE — the rule is decided on the sets, so it is tested on fabricated surfaces rather than on
/// whatever nixpkgs happens to ship today. Both directions are faults: an undeclared difference,
/// and a declaration that is no longer true.
fn surface_faults(surfaces: &[(String, std::collections::BTreeSet<String>)]) -> Vec<ClosureFault> {
    let mut faults = Vec::new();
    // Nothing to compare. The caller already holds the fault that stopped every closure being
    // read, and adding "every declaration is stale" on top of it would name the wrong file.
    if surfaces.is_empty() {
        return faults;
    }
    for (pkg, bins) in surfaces {
        if bins.is_empty() {
            faults.push(ClosureFault::EmptySurface { pkg: pkg.clone() });
            continue;
        }
        let others: std::collections::BTreeSet<&String> = surfaces
            .iter()
            .filter(|(p, _)| p != pkg)
            .flat_map(|(_, b)| b.iter())
            .collect();
        let only: Vec<&String> = bins.iter().filter(|b| !others.contains(*b)).collect();
        for bin in &only {
            if !DECLARED_ASYMMETRY
                .iter()
                .any(|d| d.only_in == pkg && d.bin == bin.as_str())
            {
                faults.push(ClosureFault::UndeclaredAsymmetry {
                    pkg: pkg.clone(),
                    bin: (*bin).clone(),
                });
            }
        }
        for d in DECLARED_ASYMMETRY.iter().filter(|d| d.only_in == pkg) {
            if !only.iter().any(|b| b.as_str() == d.bin) {
                faults.push(ClosureFault::StaleAsymmetry {
                    pkg: d.only_in,
                    bin: d.bin,
                });
            }
        }
    }
    // A declaration naming a runner that is not in the comparison is stale too — otherwise a
    // typo'd or renamed package would silently exempt nothing while looking like it exempted
    // something.
    for d in DECLARED_ASYMMETRY {
        if !surfaces.iter().any(|(p, _)| p == d.only_in) {
            faults.push(ClosureFault::StaleAsymmetry {
                pkg: d.only_in,
                bin: d.bin,
            });
        }
    }
    faults
}

/// `closure-surface`: the model runners' capability surfaces differ only where declared.
///
/// This gate cannot see a capability BOTH runners lack — #85's exact shape — which is why it is
/// the third of three and not the only one. What it does hold is the other half: a dependency
/// discovered ONCE cannot go missing from the other side silently.
fn closure_surface_mode(flake: &str) -> i32 {
    let (closures, mut faults) = model_runner_closures(flake);
    let surfaces: Vec<(String, std::collections::BTreeSet<String>)> = closures
        .iter()
        .map(|c| (c.pkg.clone(), capability_surface(c)))
        .collect();
    for (pkg, bins) in &surfaces {
        println!("  {pkg:<13} {} binaries", bins.len());
    }
    for d in DECLARED_ASYMMETRY {
        println!("  declared: {} only in {} — {}", d.bin, d.only_in, d.why);
    }
    faults.extend(surface_faults(&surfaces));
    report_closure(&faults, "capability surfaces match the declared asymmetry")
}

/// What a trace says about the run's tools, as distinct from the model's judgement.
#[derive(Debug, Default, PartialEq, Eq)]
struct ToolingReport {
    /// Files the environment listed as existing and the run then failed to read. Non-empty means
    /// the run was blind to evidence that was there — a TOOLING failure, whatever the cause.
    unreadable: Vec<String>,
    /// Bash commands that exited 127. Surfaced, never raised to a failed outcome — see below.
    command_not_found: Vec<String>,
}

/// Read a `tool_result` block's content as text, whatever shape the harness used for it.
fn tool_result_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Derive the tooling report from a trace, WITHOUT reading any error message.
///
/// The signal for `unreadable` is a relation between two tool results, not the text of either:
///
///   1. a SUCCESSFUL `Glob` result is a filesystem enumeration — every line of it is the absolute
///      path of a file that existed when the run looked;
///   2. a FAILED `Read` names its target in the typed `file_path` input;
///   3. if (2)'s path is in (1)'s enumeration, the input was valid — the file was there — so the
///      failure belongs to the environment, not to the model's choice of argument.
///
/// That is the discrimination the message text is usually used for, obtained structurally. It
/// generalises: a future harness that cannot read some new artifact type for want of some new
/// binary produces exactly this shape, with no rule about that binary anywhere.
///
/// It fails SAFE in both directions. A Read error for a path nothing enumerated (the model guessed
/// at a filename) stays an input error and is not reported. A path corroborated only by `Grep` is
/// not counted either — Grep's lines carry `path:line:` prefixes and matching them would mean
/// parsing output shape rather than reading an enumeration.
///
/// Validated against every trace this box has kept: 67 runs, ~5 months, and it fires on exactly
/// the two Reads of #85's audit PDFs — no other run in the corpus produces a corroborated Read
/// error.
///
/// `command_not_found` is the weaker sibling and is deliberately NOT part of the outcome. The
/// harness renders a failed Bash with a first line of `Exit code <n>`, so a 127 is POSIX's
/// "command not found" — typed, not prose. But the producer's Bash is a general-purpose surface
/// where the model legitimately PROBES for tools it does not need (`which node npm`, `node
/// --version`): 2 of the 5 historical 127s on this box are probes of that kind. Raising them to a
/// failed run would spend the outcome's credibility on non-failures, so they are surfaced as a
/// field for a human to read instead. (The other 3 are real: `node`, `npm` and `fc-list` are
/// undeclared in the producer's closure to this day.)
fn trace_tooling_report(content: &str) -> ToolingReport {
    let mut names: std::collections::HashMap<String, (String, Value)> =
        std::collections::HashMap::new();
    let mut listed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut failed_reads: Vec<String> = Vec::new();
    let mut report = ToolingReport::default();

    for line in content.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let blocks = ev
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                for b in blocks.into_iter().flatten() {
                    if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                        continue;
                    }
                    let (Some(id), Some(name)) = (
                        b.get("id").and_then(|i| i.as_str()),
                        b.get("name").and_then(|n| n.as_str()),
                    ) else {
                        continue;
                    };
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    names.insert(id.to_string(), (name.to_string(), input));
                }
            }
            Some("user") => {
                let blocks = ev
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                for b in blocks.into_iter().flatten() {
                    if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let id = b.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                    let Some((tool, input)) = names.get(id) else {
                        continue;
                    };
                    let is_error = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                    match (tool.as_str(), is_error) {
                        // A successful Glob IS the filesystem's own statement that these files exist.
                        ("Glob", false) => {
                            for l in tool_result_text(b).lines() {
                                let l = l.trim();
                                if l.starts_with('/') {
                                    listed.insert(l.to_string());
                                }
                            }
                        }
                        ("Read", true) => {
                            if let Some(p) = input.get("file_path").and_then(|p| p.as_str()) {
                                failed_reads.push(p.to_string());
                            }
                        }
                        ("Bash", true) => {
                            let text = tool_result_text(b);
                            let first = text.lines().next().unwrap_or("").trim();
                            if first
                                .strip_prefix("Exit code ")
                                .and_then(|n| n.trim().parse::<i32>().ok())
                                == Some(127)
                            {
                                // The COMMAND comes from the typed tool_use input, so even this
                                // human-facing detail is not scraped out of an error message.
                                let cmd = input
                                    .get("command")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(120)
                                    .collect::<String>();
                                report.command_not_found.push(cmd);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // Evaluated after the whole trace so corroboration is order-independent.
    for p in failed_reads {
        if listed.contains(&p) && !report.unreadable.contains(&p) {
            report.unreadable.push(p);
        }
    }
    report
}

/// How a run ended. This is a TYPE, not a grep over the trace bytes.
///
/// The runners used to decide model-fallback with
/// `grep -qiE '"api_error_status": ?429|reached your [^"]*limit|usage limit|session limit'`
/// across the whole trace AND its stderr sidecar. That matched anywhere — including inside a
/// tool RESULT quoting an unrelated 429, or a PR body the run happened to read — so an
/// unaffected model could be skipped for a quota problem that was never ours.
///
/// Here the discriminant is structural: only `result` events are consulted, and only their
/// typed fields. Text is read from `.result` alone (the model's own final message), never from
/// arbitrary trace bytes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TraceOutcome {
    Ok,
    /// The model is quota/usage limited — the ONLY outcome that should advance model fallback.
    QuotaLimited,
    /// The run's TOOLS could not do their job: a declared dependency was absent before the run
    /// started, or the run failed to read a file the environment had listed as existing. Not the
    /// model's mistake, and never `ok` — a blind run that answers anyway is the failure #85
    /// recorded, where the same PR came out `ready` from the run that could not read its evidence
    /// and `reject` from the run that could.
    ToolingFailure,
    Error,
}

impl TraceOutcome {
    /// The wire word written into metrics/runs.jsonl. `session-limit` is kept verbatim: the
    /// committed history already uses it and the dashboard reads it.
    fn as_str(self) -> &'static str {
        match self {
            TraceOutcome::Ok => "ok",
            TraceOutcome::QuotaLimited => "session-limit",
            TraceOutcome::ToolingFailure => "tooling-failure",
            TraceOutcome::Error => "error",
        }
    }
}

/// True when a `result` event says *this run* was refused for quota/usage limits.
///
/// Checked in order of decreasing structure:
///   1. `api_error_status` == 429 (typed, unambiguous — a real HTTP status field),
///   2. `subtype` naming a limit (the SDK's own enumerated reason),
///   3. the `result` string itself, which is the model's final message and the only place a
///      limit is reported in prose. Scoped to that one field, so a 429 quoted inside a tool
///      result or a fetched page can never trip it.
fn result_event_is_quota_limited(ev: &Value) -> bool {
    if ev.get("type").and_then(|t| t.as_str()) != Some("result") {
        return false;
    }
    // 1. Typed status field. Accept both the numeric and stringified spellings the SDK has used.
    if let Some(status) = ev.get("api_error_status") {
        let is_429 = status.as_i64() == Some(429)
            || status.as_str().map(|s| s.trim() == "429").unwrap_or(false);
        if is_429 {
            return true;
        }
    }
    // 2. Enumerated subtype.
    if let Some(sub) = ev.get("subtype").and_then(|s| s.as_str()) {
        let sub = sub.to_ascii_lowercase();
        if sub.contains("usage_limit")
            || sub.contains("session_limit")
            || sub.contains("rate_limit")
        {
            return true;
        }
    }
    // 3. The model's own final message, and nothing else.
    if let Some(text) = ev.get("result").and_then(|r| r.as_str()) {
        let t = text.to_ascii_lowercase();
        if t.contains("usage limit") || t.contains("session limit") || t.contains("reached your") {
            return true;
        }
    }
    false
}

/// Classify a whole trace. `exit_code` is the runner's own observation of the claude process, so a
/// process that died without ever emitting a `result` event is still an error rather than an "ok".
fn classify_trace(trace: &str, exit_code: i32) -> TraceOutcome {
    let quota = trace
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|ev| result_event_is_quota_limited(&ev));
    if quota {
        return TraceOutcome::QuotaLimited;
    }
    // Ordered AFTER quota so the runners' model-fallback loop still sees `session-limit` and
    // advances: a run the API refused never got far enough for its tools to matter.
    if !trace_tooling_report(trace).unreadable.is_empty() {
        return TraceOutcome::ToolingFailure;
    }
    if exit_code != 0 {
        return TraceOutcome::Error;
    }
    TraceOutcome::Ok
}

/// The whole run's outcome: what the trace says, plus what `preflight` found before the trace
/// existed.
///
/// A preflight failure has no trace to classify — the model never ran — so the fact arrives as the
/// list of binaries that would not resolve. It outranks everything else: a run that was stopped
/// before it started is neither quota-limited nor merely errored.
fn classify_outcome(trace: &str, exit_code: i32, preflight_missing: &[String]) -> TraceOutcome {
    if !preflight_missing.is_empty() {
        return TraceOutcome::ToolingFailure;
    }
    classify_trace(trace, exit_code)
}

/// `trace-outcome <trace> --exit-code <n>`: print the typed outcome word and exit 0.
///
/// The runners' model-fallback loop consumes this instead of grepping. Kept separate from
/// `run-metrics` because the loop must decide whether to try the next model *before* the run's
/// metrics line is assembled.
fn trace_outcome_mode(path: &str, exit_code: i32) -> i32 {
    // An unreadable/absent trace is not itself an error to report on: the runner already
    // distinguishes "no event stream" separately. Treat it as an empty trace.
    let content = std::fs::read_to_string(path).unwrap_or_default();
    println!("{}", classify_trace(&content, exit_code).as_str());
    0
}

/// One `{ts, counts}` rollup line for human-queue-history.jsonl, from a human-queue.json snapshot.
///
/// Replaces `jq -c --arg ts "$ts" '{ts: $ts, counts: .counts}'` in refresh-human-queue.sh and the
/// per-commit `select(.counts != null) | …` in backfill-human-queue-history.sh — one implementation
/// for the live append and the historical backfill, so the two can never drift into different line
/// shapes for the same file.
///
/// Returns None when the snapshot has no `counts` (the backfill's `select`): early commits of
/// human-queue.json predate the key, and those commits must be skipped, not emitted as null.
fn queue_history_line(snapshot: &str, ts: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(snapshot).ok()?;
    let counts = doc.get("counts")?;
    if counts.is_null() {
        return None;
    }
    let line = serde_json::json!({ "ts": ts, "counts": counts });
    Some(serde_json::to_string(&line).unwrap())
}

/// `queue-history-line [<snapshot.json>] --ts <iso8601>`: emit the rollup line, or nothing.
///
/// Reads stdin when no path is given, because the backfill feeds it `git show <sha>:human-queue.json`
/// per commit rather than a file on disk.
fn queue_history_line_mode(path: Option<&str>, ts: &str) -> i32 {
    let snapshot = match path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: cannot read snapshot {p}: {e}");
                return 2;
            }
        },
        None => {
            use std::io::Read;
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("error: cannot read snapshot from stdin: {e}");
                return 2;
            }
            buf
        }
    };
    // No counts => print nothing and succeed. The backfill loop appends our stdout directly, so a
    // skipped commit must contribute zero bytes rather than a `null` line.
    if let Some(line) = queue_history_line(&snapshot, ts) {
        println!("{line}");
    }
    0
}

/// Render one trace event as the human-readable log line the runners tee into `$LOG`.
///
/// This is the jq distiller from campaign-run.sh/review-run.sh, moved into the binary so both
/// runners share one implementation and so the truncation widths are covered by tests rather than
/// by an 8-line jq program duplicated in two shell scripts.
///
/// Widths and glyphs are preserved exactly: tool calls and assistant text clip to 200 characters,
/// result lines to 800. Clipping is by CHARACTER, matching jq's `.[0:200]` (which slices
/// codepoints), so a multi-byte glyph is never cut in half.
fn distill_event(ev: &Value) -> Vec<String> {
    fn flatten(s: &str, limit: usize) -> String {
        s.replace('\n', " ").chars().take(limit).collect()
    }
    let mut out = Vec::new();
    match ev.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            let content = ev
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());
            for item in content.into_iter().flatten() {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        // Same precedence as the jq: the command, else the description, else the
                        // whole input rendered as JSON.
                        let input = item.get("input");
                        let detail = input
                            .and_then(|i| i.get("command"))
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                input
                                    .and_then(|i| i.get("description"))
                                    .and_then(|d| d.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| match input {
                                Some(i) => serde_json::to_string(i).unwrap_or_default(),
                                None => String::new(),
                            });
                        out.push(format!("  ▸ {}  {}", name, flatten(&detail, 200)));
                    }
                    Some("text") => {
                        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        out.push(format!("  · {}", flatten(text, 200)));
                    }
                    _ => {}
                }
            }
        }
        Some("result") => {
            let subtype = ev
                .get("subtype")
                .and_then(|s| s.as_str())
                .unwrap_or("done")
                .to_ascii_uppercase();
            let result = ev.get("result").and_then(|r| r.as_str()).unwrap_or("");
            out.push(format!("  ⟹ {}: {}", subtype, flatten(result, 800)));
        }
        _ => {}
    }
    out
}

/// `distill-trace`: stream stream-json on stdin, write the human log lines on stdout.
///
/// Flushed per line, matching jq's `--unbuffered`: the runner tees this into the live log while the
/// run is still going, and a human watching `tail -f` must see progress rather than a block at exit.
/// Unparseable lines are skipped rather than fatal — the trace is written by another process and a
/// torn final line must not lose the whole distillation.
fn distill_trace_mode() -> i32 {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for out in distill_event(&ev) {
            if writeln!(stdout, "{out}").is_err() {
                return 0; // downstream closed (the runner's `|| cat >/dev/null` path)
            }
        }
        let _ = stdout.flush();
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Weekly-budget pace gate (was usage-gate.sh).
//
// Everything this gate does is data work — one HTTPS GET, a JSON parse, ISO-8601 date math and a
// threshold comparison — so it lives here rather than in bash, where `python3` was only ever
// present because bash cannot do any of it. Bash keeps process control; the binary does the rest.
//
// Two properties improve by being in-process:
//   1. The bearer token never reaches the process table. The shell version went to real lengths
//      for this (`curl --config -`, reading the header from stdin so the token stays out of argv);
//      here there is no argv to leak into, so the property holds by construction.
//   2. The pace arithmetic is a pure function of (used, reset, now, slack, ceiling) instead of a
//      heredoc, so every branch and boundary is directly testable.
// ---------------------------------------------------------------------------------------------

/// One week, in milliseconds — the budget period the gate paces against.
const USAGE_WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What the gate decided. The exit code IS the interface: the runners branch on it.
#[derive(Debug, PartialEq, Eq)]
enum UsageVerdict {
    /// Exit 0 — this tick may run.
    Run(String),
    /// Exit 10 — skip this tick. The caller logs the reason and exits 0 itself.
    Pause(String),
}

impl UsageVerdict {
    fn code(&self) -> i32 {
        match self {
            UsageVerdict::Run(_) => 0,
            UsageVerdict::Pause(_) => 10,
        }
    }
    fn reason(&self) -> &str {
        match self {
            UsageVerdict::Run(r) | UsageVerdict::Pause(r) => r,
        }
    }
}

/// A usage reading, whichever source it came from.
#[derive(Debug, PartialEq)]
struct UsageReading {
    /// Percent of the weekly budget already used.
    used: f64,
    /// When the week rolls. `None` only via the fallback path — the endpoint always carries it,
    /// and a reading whose `resets_at` will not parse is discarded rather than paced blind.
    reset_ms: Option<i64>,
    /// The reset timestamp as the source spelled it. Echoed rather than re-formatted so the log
    /// shows exactly what the API said, and so no date-formatting code sits between the two.
    reset_display: Option<String>,
    /// `endpoint` or `fallback reading` — surfaced in the reason so a run's log says which was used.
    source: &'static str,
}

/// Where usage "should" be by now at a steady burn toward `reset_ms`, as a percentage.
///
/// Clamped at both ends: a reset more than a week out yields a negative fraction (clamps to 0),
/// and one already past yields over 1 (clamps to 100). Split out from [`usage_gate_decide`]
/// because the upper clamp is unreachable through that path — the `now >= reset` branch returns
/// first — so it would otherwise be untestable.
fn linear_pct(now_ms: i64, reset_ms: i64) -> f64 {
    let frac = (now_ms - (reset_ms - USAGE_WEEK_MS)) as f64 / USAGE_WEEK_MS as f64;
    (frac * 100.0).clamp(0.0, 100.0)
}

/// The whole gate decision, as a pure function.
///
/// Order matters: the CEILING is checked before the PACE, and applies whatever the pace — being
/// under a linear burn is no help once the weekly budget is nearly spent.
///
/// `None` reading means the endpoint could not be read and no fallback is set. That is INERT —
/// it prints OK and the crons RUN. This is deliberate and must stay: an earlier version of this
/// gate could not read usage and made the operator paste percentages in by hand, which silently
/// paused both crons for 22 consecutive ticks when a reading went stale at 80%. The gate exists
/// to pace spending, not to become a new way for the pipeline to stall — so a malformed response,
/// an expired token, a network failure and an unparseable date all land here, never on Pause.
fn usage_gate_decide(
    reading: Option<&UsageReading>,
    now_ms: i64,
    slack: f64,
    ceiling: f64,
) -> UsageVerdict {
    let Some(r) = reading else {
        return UsageVerdict::Run(
            "OK: usage endpoint unreachable and no fallback reading set — gate inert".to_string(),
        );
    };
    let (used, source) = (r.used, r.source);

    // 1. CEILING — at or over pauses. `>=` not `>`: the ceiling is the point we stop, not the
    //    point we have already passed.
    if used >= ceiling {
        return UsageVerdict::Pause(format!(
            "PAUSE: {used:.0}% of the weekly budget used ({source}) — at/over the {ceiling:.0}% ceiling"
        ));
    }

    let (Some(reset_ms), Some(reset_display)) = (r.reset_ms, r.reset_display.as_deref()) else {
        return UsageVerdict::Run(format!(
            "OK: {used:.0}% used ({source}), under the {ceiling:.0}% ceiling — no reset known, pacing off"
        ));
    };

    if now_ms >= reset_ms {
        return UsageVerdict::Run(format!(
            "OK: reset {reset_display} has passed ({source}) — new week"
        ));
    }

    // 2. PACE — pause only when usage is MORE than `slack` ahead of the linear burn. Exactly at
    //    the slack boundary still runs; `slack` is an allowance, not a limit to trip on.
    let linear = linear_pct(now_ms, reset_ms);
    if used - linear > slack {
        return UsageVerdict::Pause(format!(
            "PAUSE: {used:.0}% used vs {linear:.0}% linear-by-now toward reset \
             {reset_display} ({source}) — >{slack:.0}% ahead of pace"
        ));
    }

    UsageVerdict::Run(format!(
        "OK: {used:.0}% used vs {linear:.0}% linear-by-now toward reset \
         {reset_display} ({source}) — within {slack:.0}% slack"
    ))
}

/// Pull the OAuth bearer token out of the credentials file (`.claudeAiOauth.accessToken`).
/// Every failure — absent file, bad JSON, missing key, empty value — is `None`, which routes to
/// the fallback-or-inert path rather than an error.
fn oauth_token(creds: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(creds).ok()?).ok()?;
    let tok = doc.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
    (!tok.is_empty()).then(|| tok.to_string())
}

/// The `seven_day` block: `utilization` is the percent used, `resets_at` is authoritative.
/// Both must be present and well-formed or the whole reading is discarded — pacing against a
/// usage number with no trustworthy reset date would be guessing.
fn parse_seven_day(raw: &str) -> Option<UsageReading> {
    let week = serde_json::from_str::<Value>(raw)
        .ok()?
        .get("seven_day")?
        .clone();
    let used = week.get("utilization")?.as_f64()?;
    let resets_at = week.get("resets_at")?.as_str()?.to_string();
    let reset_ms = iso_to_epoch_ms(&resets_at)?;
    Some(UsageReading {
        used,
        reset_ms: Some(reset_ms),
        reset_display: Some(resets_at),
        source: "endpoint",
    })
}

/// The operator's last manual reading, used only when the endpoint gave nothing.
///
/// A `USAGE_RESET_AT` that is set but unparseable discards the whole reading (returns `None`,
/// i.e. inert) rather than pacing with no reset — matching the shell version, where the date
/// parse and the percent parse shared one error path.
fn fallback_reading(used_pct: &str, reset_at: &str) -> Option<UsageReading> {
    if used_pct.trim().is_empty() {
        return None;
    }
    let used: f64 = used_pct.trim().parse().ok()?;
    let (reset_ms, reset_display) = if reset_at.trim().is_empty() {
        (None, None)
    } else {
        (
            Some(iso_to_epoch_ms(reset_at.trim())?),
            Some(reset_at.trim().to_string()),
        )
    };
    Some(UsageReading {
        used,
        reset_ms,
        reset_display,
        source: "fallback reading",
    })
}

/// One HTTPS GET for the usage document. Any failure is `None` — see [`usage_gate_decide`] for
/// why nothing here may escalate to a pause.
fn fetch_usage(url: &str, token: &str) -> Option<String> {
    ureq::get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()
}

/// Read an env var as f64, falling back when unset, empty or unparseable.
fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// `usage-gate`: prints one line and exits 0 (run) or 10 (pause).
///
/// Config is env-only, exported by the runners from cron.env — the same way ORGS/PR_ASSIGNEE
/// already reach this binary.
fn usage_gate_mode() -> i32 {
    let ceiling = env_f64("USAGE_CEILING_PCT", 90.0);
    let slack = env_f64("USAGE_SLACK_PCT", 5.0);

    let creds = std::env::var("CLAUDE_CREDENTIALS").unwrap_or_else(|_| {
        format!(
            "{}/.claude/.credentials.json",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let url = std::env::var("USAGE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/api/oauth/usage".to_string());

    let reading = oauth_token(&creds)
        .and_then(|tok| fetch_usage(&url, &tok))
        .and_then(|body| parse_seven_day(&body))
        .or_else(|| {
            fallback_reading(
                &std::env::var("USAGE_USED_PCT").unwrap_or_default(),
                &std::env::var("USAGE_RESET_AT").unwrap_or_default(),
            )
        });

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let verdict = usage_gate_decide(reading.as_ref(), now_ms, slack, ceiling);
    println!("{}", verdict.reason());
    verdict.code()
}

#[cfg(test)]
mod usage_gate_tests {
    use super::*;

    fn ms(iso: &str) -> i64 {
        iso_to_epoch_ms(iso).expect("test timestamp must parse")
    }
    /// A reading from the endpoint with a known reset.
    fn reading(used: f64, reset: &str) -> UsageReading {
        UsageReading {
            used,
            reset_ms: Some(ms(reset)),
            reset_display: Some(reset.to_string()),
            source: "endpoint",
        }
    }
    const RESET: &str = "2026-07-19T00:00:00Z";

    // ---- inert: the property that must never regress to fail-closed --------------------------

    // No endpoint and no fallback => RUN. An earlier gate that could not read usage paused both
    // crons for 22 consecutive ticks; this gate paces spending, it does not stall the pipeline.
    #[test]
    fn no_reading_is_inert_and_runs() {
        let v = usage_gate_decide(None, ms("2026-07-15T00:00:00Z"), 5.0, 90.0);
        assert_eq!(v.code(), 0);
        assert!(v.reason().starts_with("OK:"), "{}", v.reason());
        assert!(v.reason().contains("gate inert"), "{}", v.reason());
    }

    // Every unreadable-usage shape collapses to None, i.e. to the inert path above — never a pause.
    #[test]
    fn unreadable_usage_shapes_never_produce_a_reading() {
        assert!(parse_seven_day("").is_none(), "empty body");
        assert!(parse_seven_day("not json").is_none(), "malformed JSON");
        assert!(parse_seven_day("{}").is_none(), "missing seven_day");
        assert!(
            parse_seven_day(r#"{"seven_day":{"resets_at":"2026-07-19T00:00:00Z"}}"#).is_none(),
            "missing utilization"
        );
        assert!(
            parse_seven_day(r#"{"seven_day":{"utilization":50.0}}"#).is_none(),
            "missing resets_at"
        );
        assert!(
            parse_seven_day(r#"{"seven_day":{"utilization":50.0,"resets_at":"not-a-date"}}"#)
                .is_none(),
            "unparseable resets_at must discard the whole reading, not pace blind"
        );
    }

    // ---- ceiling ------------------------------------------------------------------------------

    // AT the ceiling pauses: the ceiling is where we stop, not where we have already gone past.
    // Kills `used >= ceiling` -> `used > ceiling`.
    #[test]
    fn ceiling_pauses_at_the_boundary_not_only_over_it() {
        let at = usage_gate_decide(
            Some(&reading(90.0, RESET)),
            ms("2026-07-18T00:00:00Z"),
            5.0,
            90.0,
        );
        assert_eq!(
            at.code(),
            10,
            "exactly at the ceiling must PAUSE: {}",
            at.reason()
        );
        assert!(
            at.reason().contains("at/over the 90% ceiling"),
            "{}",
            at.reason()
        );

        // Just under the ceiling falls through to the pace check instead.
        let under = usage_gate_decide(
            Some(&reading(89.0, RESET)),
            ms("2026-07-18T00:00:00Z"),
            5.0,
            90.0,
        );
        assert!(
            !under.reason().contains("ceiling —"),
            "89 < 90 must not trip the ceiling: {}",
            under.reason()
        );
    }

    // The ceiling is checked BEFORE the pace and applies whatever the pace says. Here usage is
    // comfortably BEHIND a linear burn (95 used vs ~99 linear), so a pace-first ordering would
    // return Run. Kills any reordering of the two checks.
    #[test]
    fn ceiling_is_checked_before_pace_and_wins() {
        // One hour before reset => linear ~99.4%, so used-linear is negative: pace alone says run.
        let now = ms(RESET) - 3_600_000;
        let pace_only = usage_gate_decide(Some(&reading(95.0, RESET)), now, 5.0, 100.0);
        assert_eq!(pace_only.code(), 0, "precondition: pace alone would RUN");

        let v = usage_gate_decide(Some(&reading(95.0, RESET)), now, 5.0, 90.0);
        assert_eq!(v.code(), 10, "ceiling must win over a healthy pace");
        assert!(v.reason().contains("ceiling"), "{}", v.reason());

        // The ceiling is checked before EVERY later branch, not just the pace one — the two
        // branches that return Run early would otherwise swallow it. Without these, moving the
        // ceiling check below the pace check still passes, because in the case above both checks
        // fire and only their order differs.

        // (a) Over the ceiling with NO reset known: still a pause, not "pacing off".
        let no_reset = UsageReading {
            used: 95.0,
            reset_ms: None,
            reset_display: None,
            source: "fallback reading",
        };
        let v = usage_gate_decide(Some(&no_reset), now, 5.0, 90.0);
        assert_eq!(
            v.code(),
            10,
            "over the ceiling must pause even with pacing off: {}",
            v.reason()
        );

        // (b) Over the ceiling just after the week rolled: still a pause, not "new week".
        let v = usage_gate_decide(Some(&reading(95.0, RESET)), ms(RESET) + 1, 5.0, 90.0);
        assert_eq!(
            v.code(),
            10,
            "over the ceiling must pause even once the reset has passed: {}",
            v.reason()
        );
    }

    // ---- pace ---------------------------------------------------------------------------------

    // Exactly `slack` ahead still RUNS — slack is an allowance, not a limit to trip on.
    // Kills `used - linear > slack` -> `>= slack`.
    #[test]
    fn pace_boundary_runs_at_exactly_slack_and_pauses_just_past_it() {
        let now = ms(RESET) - USAGE_WEEK_MS / 2; // half the week elapsed => linear = 50%
        assert_eq!(linear_pct(now, ms(RESET)), 50.0, "precondition");

        let at = usage_gate_decide(Some(&reading(55.0, RESET)), now, 5.0, 90.0);
        assert_eq!(
            at.code(),
            0,
            "exactly 5 ahead is within slack: {}",
            at.reason()
        );
        assert!(at.reason().contains("within 5% slack"), "{}", at.reason());

        let over = usage_gate_decide(Some(&reading(55.1, RESET)), now, 5.0, 90.0);
        assert_eq!(
            over.code(),
            10,
            "5.1 ahead exceeds slack: {}",
            over.reason()
        );
        assert!(over.reason().contains("ahead of pace"), "{}", over.reason());
    }

    // Behind pace runs, and the reason names both numbers and the source.
    #[test]
    fn behind_pace_runs_and_reports_both_numbers() {
        let now = ms(RESET) - USAGE_WEEK_MS / 2;
        let v = usage_gate_decide(Some(&reading(10.0, RESET)), now, 5.0, 90.0);
        assert_eq!(v.code(), 0);
        assert!(v.reason().contains("10% used"), "{}", v.reason());
        assert!(v.reason().contains("50% linear-by-now"), "{}", v.reason());
        assert!(v.reason().contains("(endpoint)"), "{}", v.reason());
    }

    // ---- the linear-pace clamp -----------------------------------------------------------------

    // Both ends. The upper clamp is unreachable through usage_gate_decide (the `now >= reset`
    // branch returns first), which is exactly why linear_pct is its own function.
    #[test]
    fn linear_pct_clamps_at_both_ends() {
        let reset = ms(RESET);
        // Reset two weeks out => frac = -1 => clamps to 0, not negative.
        assert_eq!(linear_pct(reset - 2 * USAGE_WEEK_MS, reset), 0.0);
        // Reset already a week past => frac = 2 => clamps to 100, not 200.
        assert_eq!(linear_pct(reset + USAGE_WEEK_MS, reset), 100.0);
        // And it is linear in between.
        assert_eq!(linear_pct(reset - USAGE_WEEK_MS, reset), 0.0);
        assert_eq!(linear_pct(reset - USAGE_WEEK_MS / 4, reset), 75.0);
        assert_eq!(linear_pct(reset, reset), 100.0);
    }

    // ---- week roll and missing reset ------------------------------------------------------------

    #[test]
    fn reset_already_passed_runs_as_a_new_week() {
        // Usage is high AND way ahead of any pace, but the week has rolled, so it must RUN.
        let v = usage_gate_decide(Some(&reading(88.0, RESET)), ms(RESET) + 1, 5.0, 90.0);
        assert_eq!(v.code(), 0, "{}", v.reason());
        assert!(v.reason().contains("new week"), "{}", v.reason());
        assert!(
            v.reason().contains(RESET),
            "reason echoes the reset: {}",
            v.reason()
        );

        // EXACTLY at the reset instant is already the new week. Pinned because `>=` -> `>` here
        // still returns code 0 (linear clamps to 100, so nothing reads as "ahead of pace") — only
        // the REASON distinguishes them, and a run logged as a pace result at the roll boundary
        // would misreport why it ran.
        let at = usage_gate_decide(Some(&reading(88.0, RESET)), ms(RESET), 5.0, 90.0);
        assert_eq!(at.code(), 0, "{}", at.reason());
        assert!(
            at.reason().contains("new week"),
            "at the reset instant the week has rolled: {}",
            at.reason()
        );
    }

    // A reading with no reset paces nothing, but still honours the ceiling (checked earlier).
    #[test]
    fn absent_reset_runs_with_pacing_off() {
        let r = UsageReading {
            used: 40.0,
            reset_ms: None,
            reset_display: None,
            source: "fallback reading",
        };
        let v = usage_gate_decide(Some(&r), ms("2026-07-15T00:00:00Z"), 5.0, 90.0);
        assert_eq!(v.code(), 0);
        assert!(v.reason().contains("pacing off"), "{}", v.reason());
        assert!(v.reason().contains("(fallback reading)"), "{}", v.reason());
    }

    // ---- sources -------------------------------------------------------------------------------

    #[test]
    fn endpoint_reading_parses_and_names_its_source() {
        let r = parse_seven_day(
            r#"{"seven_day":{"utilization":12.5,"resets_at":"2026-07-19T11:59:59Z"}}"#,
        )
        .expect("well-formed body");
        assert_eq!(r.used, 12.5);
        assert_eq!(r.source, "endpoint");
        assert_eq!(r.reset_display.as_deref(), Some("2026-07-19T11:59:59Z"));
        assert_eq!(r.reset_ms, iso_to_epoch_ms("2026-07-19T11:59:59Z"));
    }

    #[test]
    fn fallback_is_used_only_when_set_and_says_so() {
        assert!(fallback_reading("", "").is_none(), "unset => inert");
        assert!(fallback_reading("  ", "").is_none(), "blank => inert");
        assert!(
            fallback_reading("not-a-number", "").is_none(),
            "unparseable pct => inert"
        );
        // Set but with an unparseable reset: discard the whole reading rather than pace blind.
        assert!(
            fallback_reading("60", "not-a-date").is_none(),
            "unparseable reset must not silently become pacing-off"
        );

        let r = fallback_reading("60", RESET).expect("valid fallback");
        assert_eq!(r.used, 60.0);
        assert_eq!(r.source, "fallback reading");
        assert_eq!(r.reset_ms, iso_to_epoch_ms(RESET));

        // Percent alone is legitimate: pacing off, ceiling still enforced.
        let r = fallback_reading("60", "").expect("pct-only fallback");
        assert_eq!(r.reset_ms, None);
    }

    // The one-line reason must always say which source it used, on every path that has a reading.
    #[test]
    fn every_reading_path_names_its_source() {
        let now = ms("2026-07-15T00:00:00Z");
        for src in ["endpoint", "fallback reading"] {
            let over = UsageReading {
                used: 99.0,
                reset_ms: Some(ms(RESET)),
                reset_display: Some(RESET.to_string()),
                source: src,
            };
            assert!(
                usage_gate_decide(Some(&over), now, 5.0, 90.0)
                    .reason()
                    .contains(src),
                "ceiling path must name {src}"
            );
            let paced = UsageReading { used: 80.0, ..over };
            assert!(
                usage_gate_decide(Some(&paced), now, 5.0, 90.0)
                    .reason()
                    .contains(src),
                "pace path must name {src}"
            );
        }
    }

    #[test]
    fn oauth_token_is_none_for_every_bad_shape() {
        let dir = std::env::temp_dir().join(format!("prr-usage-gate-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p.to_string_lossy().to_string()
        };
        assert!(
            oauth_token("/definitely/not/here.json").is_none(),
            "absent file"
        );
        assert!(
            oauth_token(&write("bad.json", "not json")).is_none(),
            "bad JSON"
        );
        assert!(
            oauth_token(&write("nokey.json", "{}")).is_none(),
            "missing claudeAiOauth"
        );
        assert!(
            oauth_token(&write("notok.json", r#"{"claudeAiOauth":{}}"#)).is_none(),
            "missing accessToken"
        );
        assert!(
            oauth_token(&write(
                "empty.json",
                r#"{"claudeAiOauth":{"accessToken":""}}"#
            ))
            .is_none(),
            "empty token is not a token"
        );
        assert_eq!(
            oauth_token(&write(
                "ok.json",
                r#"{"claudeAiOauth":{"accessToken":"sk-abc"}}"#
            )),
            Some("sk-abc".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_codes_are_the_interface() {
        assert_eq!(UsageVerdict::Run(String::new()).code(), 0);
        assert_eq!(UsageVerdict::Pause(String::new()).code(), 10);
    }
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
///
/// Thin CLI shell over [`record_verdict_apply`]: it OWNS the printing so the core can be reused by a
/// caller that must not write to stdout (the MCP server — a stray stdout line corrupts its JSON-RPC
/// stream). Exit codes are unchanged: 0 ok, 1 error, 2 usage, 3 human-decision refusal.
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
    match record_verdict_apply(slug, pr, verdict, note, cost, basis, dry_run) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err((code, msg)) => {
            eprintln!("{msg}");
            code
        }
    }
}

/// The verdict write itself. Returns the human-readable success report, or `(exit code, message)`.
/// Writes NOTHING to stdout — non-fatal warnings still go to stderr, which is safe for every caller.
#[allow(clippy::too_many_arguments)]
fn record_verdict_apply(
    slug: &str,
    pr: &str,
    verdict: &str,
    note: &str,
    cost: Option<i64>,
    basis: &str,
    dry_run: bool,
) -> Result<String, (i32, String)> {
    let Some(target) = verdict_label(verdict) else {
        return Err((2, "usage: pr-review-report record-verdict <owner/repo> <pr> <ready|reject|design|close|relink> [note...] [--cost <n>] [--basis <s>] [--dry-run]".to_string()));
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
        return Err((
            1,
            format!("error: `gh pr view {slug}#{pr}` failed — not writing on incomplete data"),
        ));
    };
    let (to_remove, has_target, sha, skip) = match verdict_plan(&pr_json, target, verdict) {
        VerdictPlan::RefuseHuman => {
            return Err((
                3,
                format!("human verdict present on {slug}#{pr}; not overriding"),
            ));
        }
        VerdictPlan::NoSha => {
            return Err((
                1,
                format!("error: {slug}#{pr} has no head sha (headRefOid) — not recording a verdict without one"),
            ));
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
        return Ok(format!(
            "[dry-run] {slug}#{pr} @ {sha}\n  target label: {target}{}\n  labels to remove: {}\n  comment: {}\n  cost: {}",
            if has_target { " (already present)" } else { "" },
            if to_remove.is_empty() {
                "(none)".to_string()
            } else {
                to_remove.join(", ")
            },
            if skip {
                "skip (same verdict + sha already posted)".to_string()
            } else {
                format!("post -> {}", comment.replace('\n', " / "))
            },
            match cost {
                Some(c) => format!("{c} ({basis}) -> embedded in the comment"),
                None => "(none)".to_string(),
            }
        ));
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
        return Err((1, format!("error: failed to add {target} to {slug}#{pr}")));
    }
    for r in &to_remove {
        if !gh_run(&["pr", "edit", pr, "-R", slug, "--remove-label", r]) {
            eprintln!("warning: failed to remove label {r} from {slug}#{pr}");
        }
    }
    // A swallowed comment failure would report success with the SHA-bound rationale never posted.
    // The cost now travels INSIDE this comment (verdict_comment embeds it) — there is no cost sidecar.
    if !skip && !gh_run(&["pr", "comment", pr, "-R", slug, "--body", &comment]) {
        return Err((
            1,
            format!(
                "error: recorded {target} on {slug}#{pr} but FAILED to post the verdict comment"
            ),
        ));
    }
    Ok(format!(
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
    ))
}

/// Pure plan for `--flag-close-candidate`: given the issue's live state, decide what to do.
#[derive(Debug, PartialEq)]
enum CloseFlagPlan {
    AlreadyClosed,
    RefuseHuman,
    Flag { add_label: bool, post_comment: bool },
}

/// The human's namespace on an ISSUE. A ruling here is sacred: neither the producer's flag nor the
/// vetter's verdict may overwrite it.
///
/// `human:keep-open` is retained for compatibility but is NOT a label the org actually uses — the
/// real set is the same three [`has_human_override`] enforces on PRs. Checking only `keep-open` +
/// `close-candidate` (as this did) left a `human:reject` / `human:design` ruling on an issue
/// invisible here, so the producer could flag an issue a human had already parked.
const HUMAN_RULING_LABELS: [&str; 4] = [
    "human:reject",
    "human:design",
    "human:close-candidate",
    "human:keep-open",
];

/// Does any human ruling sit on this issue? Takes label NAMES, which callers already have.
fn has_human_ruling(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|l| HUMAN_RULING_LABELS.contains(&l.as_str()))
}

/// A human ruling is sacred (refuse); a CLOSED issue is moot; otherwise flag it, adding the label /
/// posting the note only when not already present.
fn close_candidate_plan(state: &str, labels: &[String], already_noted: bool) -> CloseFlagPlan {
    if state == "CLOSED" {
        return CloseFlagPlan::AlreadyClosed;
    }
    if has_human_ruling(labels) {
        return CloseFlagPlan::RefuseHuman;
    }
    CloseFlagPlan::Flag {
        add_label: !labels.iter().any(|l| l == "ai:close-candidate"),
        post_comment: !already_noted,
    }
}

/// The vetter's verdicts on a producer close-candidate FLAG. Two, because the flag is a binary
/// claim: either the evidence supports closing (leave it queued for the human) or it does not
/// (drop the flag, returning the issue to the producer's uncovered queue).
const CC_VERDICTS: [&str; 2] = ["uphold", "reject"];

/// The producer's most recent trusted close-candidate flag on an issue, as `(createdAt, body)`.
///
/// Trusted by AUTHOR, never by marker text: `🤖 ai:producer` is public body text anyone can post,
/// so an untrusted "flag" must never be judged as though the pipeline made it.
fn last_close_candidate_flag(issue: &Value) -> Option<(String, String)> {
    issue
        .get("comments")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter(|c| author_login(c) == Some(TRUSTED_AUTHOR))
        .filter_map(|c| {
            let body = c.get("body").and_then(|b| b.as_str())?;
            if !body.starts_with("🤖 ai:producer") || !body.contains("Close-candidate:") {
                return None;
            }
            let at = c.get("createdAt").and_then(|t| t.as_str()).unwrap_or("");
            Some((at.to_string(), body.to_string()))
        })
        .next_back()
}

/// The most-recent trusted `🤖 ai:vetter` comment on an ISSUE recording a close-candidate verdict.
fn last_cc_vetter_comment(issue: &Value) -> Option<String> {
    trusted_comments(issue, Some("🤖 ai:vetter"))
        .into_iter()
        .rfind(|b| b.contains("Reviewed close-candidate @"))
}

/// The issue-side analogue of [`vetted_at_head`]: a flag is vetted only when the vetter's own
/// comment pins the CURRENT flag's timestamp. The `ai:close-candidate` label alone is not a verdict
/// — it is the producer's claim. A RE-flag (new comment, new timestamp) un-vets, exactly as a moved
/// head does for a PR, so new evidence is re-judged instead of inheriting a stale pass.
fn cc_vetted_at_flag(issue: &Value, flag_at: &str) -> bool {
    !flag_at.is_empty()
        && last_cc_vetter_comment(issue)
            .map(|b| b.contains(&format!("Reviewed close-candidate @{flag_at}:")))
            .unwrap_or(false)
}

/// The sha-bound verdict comment's issue-side twin: pins the flag being judged, so the record says
/// WHICH claim was reviewed rather than merely that a review happened.
fn cc_verdict_comment(flag_at: &str, verdict: &str, note: &str) -> String {
    let tail = if note.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", note.trim())
    };
    format!("🤖 ai:vetter\nReviewed close-candidate @{flag_at}: {verdict}{tail}")
}

/// The close-candidate recording decision, computed PURELY from the fetched issue JSON — the same
/// guard-before-write shape as [`VerdictPlan`].
#[derive(Debug, PartialEq)]
enum CcVerdictPlan {
    /// A human already ruled: never overwritten.
    RefuseHuman,
    /// The issue is closed — the flag is moot.
    AlreadyClosed,
    /// No trusted producer flag to judge (nothing was claimed).
    NoFlag,
    Record {
        flag_at: String,
        /// `ai:close-candidate` is present and this verdict drops it (reject only).
        remove_label: bool,
        /// The same verdict at the same flag is already recorded — a no-op re-review.
        skip_comment: bool,
    },
}

/// PURE: may the vetter record `verdict` on this flagged issue, and what does it change?
fn cc_verdict_plan(issue_json: &Value, verdict: &str) -> CcVerdictPlan {
    let state = issue_json
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    if state == "CLOSED" {
        return CcVerdictPlan::AlreadyClosed;
    }
    let labels = label_names(issue_json);
    if has_human_ruling(&labels) {
        return CcVerdictPlan::RefuseHuman;
    }
    let Some((flag_at, _)) = last_close_candidate_flag(issue_json) else {
        return CcVerdictPlan::NoFlag;
    };
    let has_flag_label = labels.iter().any(|l| l == "ai:close-candidate");
    CcVerdictPlan::Record {
        skip_comment: last_cc_vetter_comment(issue_json)
            .map(|b| b.contains(&format!("Reviewed close-candidate @{flag_at}: {verdict}")))
            .unwrap_or(false),
        // Only `reject` mutates the label — an upheld flag stays queued for the human untouched.
        remove_label: verdict == "reject" && has_flag_label,
        flag_at,
    }
}

/// The datable anchor an `already-fixed-on-main:` reason must carry, so "the fix is on main" can be
/// checked against "the fix landed AFTER the bug was reported".
#[derive(Debug, PartialEq)]
enum FixAnchor {
    /// The reason is not an `already-fixed-on-main` claim — recency does not apply.
    NotApplicable,
    /// An `already-fixed-on-main` claim carrying nothing datable (a bare `file:line`).
    Missing,
    Commit(String),
    Pr(String),
}

/// Extract the datable anchor from a close-candidate reason.
///
/// `already-fixed-on-main` is the one category whose evidence is checkable: a commit or a merged PR
/// carries a date, so the claim can be tested against the issue's own creation date. A bare
/// `file:line on main` cannot — code that was already there when the bug was filed proves nothing,
/// which is exactly how live issues got flagged (raindex#512, #531, #549). The other categories
/// (`invalid` / `duplicate` / `wont-fix`) are judgements, not landings, so they are left alone.
fn already_fixed_anchor(reason: &str) -> FixAnchor {
    if !reason.trim_start().starts_with("already-fixed-on-main") {
        return FixAnchor::NotApplicable;
    }
    // A PR reference: any `#` followed by digits — `#123`, `PR#123`, `owner/repo#123`. Requiring
    // digits is the whole guard: `foo#bar` has none, so prose cannot masquerade as a reference.
    for (i, _) in reason.match_indices('#') {
        let digits: String = reason[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return FixAnchor::Pr(digits);
        }
    }
    // A commit sha: a standalone 7..=40 char hex run containing at least one a-f letter. The
    // letter is what separates a sha from a bare number — `20240401` (a date) and `1234567` (an
    // id) are valid hex too, and classifying them as commits would send the caller to
    // `gh api .../commits/<digits>`, which 404s and reports a confusing "could not resolve a
    // date" instead of the accurate "no usable anchor". A pure-decimal short sha is possible but
    // rare, and the cost is only that the producer must cite the PR or a longer sha.
    for word in reason.split(|c: char| !c.is_ascii_alphanumeric()) {
        if (7..=40).contains(&word.len())
            && word.chars().all(|c| c.is_ascii_hexdigit())
            && word.chars().any(|c| c.is_ascii_alphabetic())
        {
            return FixAnchor::Commit(word.to_string());
        }
    }
    FixAnchor::Missing
}

/// Is `landed` strictly after `filed`? Both are ISO-8601 (`2024-04-01T11:06:35Z`), which sorts
/// lexicographically, so a string compare is the whole comparison. `None` when either is unparseable
/// — the caller fails closed rather than guessing.
fn landed_after_filed(landed: &str, filed: &str) -> Option<bool> {
    if landed.len() < 20 || filed.len() < 20 || !landed.contains('T') || !filed.contains('T') {
        return None;
    }
    Some(landed > filed)
}

/// Enforce the recency contract on an `already-fixed-on-main` reason. Returns 0 to proceed, or a
/// non-zero exit code: 4 = the claim is unsupported (no datable anchor, or the anchor predates the
/// issue), 1 = the anchor's date could not be resolved. Fails CLOSED on an unresolvable anchor, the
/// same rule the caller applies to an unreadable issue — a wrong `already-fixed-on-main` flag on a
/// LIVE bug invites a human to close a real defect on false evidence.
fn already_fixed_recency_gate(slug: &str, issue: &str, reason: &str, issue_json: &Value) -> i32 {
    let anchor = already_fixed_anchor(reason);
    if anchor == FixAnchor::NotApplicable {
        return 0;
    }
    let filed = issue_json
        .get("createdAt")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if filed.is_empty() {
        eprintln!("error: {slug}#{issue} has no createdAt — cannot check the fix postdates it");
        return 1;
    }
    let (landed, what) = match &anchor {
        FixAnchor::Missing => {
            eprintln!(
                "refusing to flag {slug}#{issue}: an `already-fixed-on-main` reason must cite a \
                 MERGED commit sha or PR number, not only a file:line.\n\
                 A file:line proves the code is on main today, not that it landed after the issue \
                 was filed ({filed}) — code that predates the report cannot be the fix.\n\
                 Cite the commit/PR that fixed it, or use `invalid`/`duplicate`/`wont-fix` if that \
                 is the real category."
            );
            return 4;
        }
        FixAnchor::Commit(sha) => (
            gh_json(&["api", &format!("repos/{slug}/commits/{sha}")])
                .and_then(|c| {
                    c.pointer("/commit/committer/date")
                        .or_else(|| c.pointer("/commit/author/date"))
                        .and_then(|d| d.as_str())
                        .map(String::from)
                })
                .unwrap_or_default(),
            format!("commit {sha}"),
        ),
        FixAnchor::Pr(n) => (
            gh_json(&["pr", "view", n, "-R", slug, "--json", "mergedAt"])
                .and_then(|p| p.get("mergedAt").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or_default(),
            format!("PR #{n}"),
        ),
        FixAnchor::NotApplicable => unreachable!("returned above"),
    };
    recency_exit_code(slug, issue, &what, &landed, filed)
}

/// PURE: the verdict once an anchor's landing date has been looked up. 0 = the cited fix postdates
/// the report, so the flag may proceed; 4 = the evidence predates it and the claim is unsupported;
/// 1 = no usable date at all (unmerged PR, failed lookup, or a pair that cannot be ordered), which
/// fails CLOSED. Split out of `already_fixed_recency_gate` so every arm is reachable without a
/// network round trip — the "proceed" arm most of all: a gate that refuses everything looks exactly
/// as green as one that works.
fn recency_exit_code(slug: &str, issue: &str, what: &str, landed: &str, filed: &str) -> i32 {
    if landed.is_empty() {
        eprintln!(
            "error: could not resolve a date for {what} in {slug} (unmerged, or the lookup \
             failed) — not writing on unverified evidence"
        );
        return 1;
    }
    match landed_after_filed(landed, filed) {
        Some(true) => 0,
        Some(false) => {
            eprintln!(
                "refusing to flag {slug}#{issue}: {what} landed {landed}, but the issue was filed \
                 {filed}.\n\
                 Evidence that predates the report cannot be the fix — find the change that \
                 actually resolved it, or use `invalid`/`duplicate`/`wont-fix`."
            );
            4
        }
        None => {
            eprintln!("error: unparseable dates ({what}: {landed:?}, issue: {filed:?})");
            1
        }
    }
}

/// `--flag-close-candidate <owner/repo> <issue> "<reason>" [--dry-run]`: the SOLE sanctioned way the
/// producer flags a closeable ISSUE — applies the `ai:close-candidate` label + a trusted
/// `🤖 ai:producer` reason comment, replacing the old local close-candidates.jsonl. GitHub state is
/// the source of truth: a closed/fixed issue drops out of the `--state open` query automatically,
/// re-flagging is idempotent, and a human `human:keep-open` / `human:close-candidate` ruling is
/// sacred (the tool refuses, exit 3). The producer NEVER closes the issue — a human does that.
///
/// An `already-fixed-on-main` reason additionally must carry a commit sha or merged PR number whose
/// date POST-DATES the issue (exit 4). "This code is on main today" is not the same claim as "this
/// issue is fixed": evidence that predates the report cannot be the fix.
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
        "state,labels,comments,createdAt",
    ]) else {
        eprintln!("error: `gh issue view {slug}#{issue}` failed — not writing on incomplete data");
        return 1;
    };
    // ANY non-zero verdict from the gate is the answer — a numeric range here would silently let a
    // future exit code fall through into "flag it anyway", the fail-OPEN direction.
    let gate = already_fixed_recency_gate(slug, issue, reason, &j);
    if gate != 0 {
        return gate;
    }
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

    // Close-candidate vet state, as `(unvetted, upheld)`. Computed from the same state-load the
    // vetter reads, so the dashboard and the vetter can never disagree about the size of the inbox.
    // Additive and FAILURE-TOLERANT: this costs one `gh issue view` per flagged issue, so a transient
    // API failure yields (0, 0) rather than aborting the queue render or corrupting the legacy
    // `closeCandidateIssues` count, which keeps its own cheap search above.
    //
    // Upheld is derived, not stored: a REJECTED flag has its label stripped, so it cannot appear in
    // this search at all — an issue that still carries the label AND is vetted at its current flag
    // was necessarily upheld.
    // `include_skipped` is required: the upheld set lives in the skipped rows. Arrays and counts
    // both come from this ONE document, so `counts.X == X.len()` holds by construction — the
    // dashboard's boxes are click-through, and a count that disagreed with its list would render a
    // number that lists a different number of issues.
    // UNBOUNDED (`limit: None`): the dashboard renders whole sets, not a page. Paging here would
    // make `counts.closeCandidateUnvetted` disagree with the list it labels.
    let (cc_unvetted_items, cc_upheld_items) = unvetted_close_candidates_fetch(true, None)
        .ok()
        .map(|d| cc_item_arrays(&d))
        .unwrap_or_default();
    // Array and count come from one call each, so the emitted count is always the emitted array's
    // length — see `issue_state_pair`.
    let (cc_unvetted, cc_unvetted_n) = issue_state_pair(cc_unvetted_items);
    let (cc_upheld, cc_upheld_n) = issue_state_pair(cc_upheld_items);

    // Producer untouched BACKLOG: open issues with no covering open PR that are not human-gated /
    // close-candidate — the biggest bucket of the producer's inbox (work it hasn't picked up yet),
    // previously invisible on the FSM dashboard. Same coverage computation as `uncovered-issues`
    // (via `coverage_uncovered`); `is_producer_backlog` narrows it to the producer's share. A gh
    // failure leaves it empty rather than aborting the whole queue render — it is additive.
    let backlog: Vec<(String, u64, String)> = coverage_uncovered()
        .map(|(open, meta)| {
            open.iter()
                .filter(|k| meta.get(*k).map(is_producer_backlog).unwrap_or(true))
                .map(|(slug, num)| {
                    let title = meta
                        .get(&(slug.clone(), *num))
                        .and_then(|m| m.get("title"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    (slug.clone(), *num, title)
                })
                .collect()
        })
        .unwrap_or_default();

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
            // Same key at top level (the ITEM ARRAY) and under `counts` (its length), exactly as
            // `closeCandidateIssues` / `uncoveredIssues` do — the dashboard boxes are click-through.
            "closeCandidateUnvetted": cc_unvetted,
            "closeCandidateUpheld": cc_upheld,
            "uncoveredIssues": backlog.iter().map(|(s,n,t)| serde_json::json!({"repo": s, "number": n, "title": t})).collect::<Vec<_>>(),
            "leaks": leaks.iter().map(|(s,n,t,r)| serde_json::json!({"repo": s, "number": n, "title": t, "reason": r})).collect::<Vec<_>>(),
            "counts": {
                // Legacy label-based counts (UNCHANGED — the dashboard reads these).
                "ready": buckets.get("ai:ready").map(|v| v.len()).unwrap_or(0),
                "design": buckets.get("ai:design").map(|v| v.len()).unwrap_or(0),
                "blockedDeploy": buckets.get("ai:blocked-deploy").map(|v| v.len()).unwrap_or(0),
                "blockedInfra": buckets.get("ai:blocked-infra").map(|v| v.len()).unwrap_or(0),
                "blockedOn": buckets.get("ai:blocked-on").map(|v| v.len()).unwrap_or(0),
                "closeCandidateIssues": close_issues.len(),
                // Close-candidate VET lifecycle (#72/#73). `closeCandidateIssues` above keeps its
                // meaning — every issue carrying the label — and these split it by vet state:
                //   unvetted = the vetter's inbox (flagged, no human ruling, no verdict at THIS flag)
                //   upheld   = the vetter judged the evidence sound; genuinely queued for the human
                // A REJECTED flag needs no key: the vetter strips `ai:close-candidate`, so the issue
                // leaves this set entirely and reappears under `uncoveredIssues`.
                "closeCandidateUnvetted": cc_unvetted_n,
                "closeCandidateUpheld": cc_upheld_n,
                "leaks": leaks.len(),
                "totalProducerPrs": prs.len(),
                // Producer untouched backlog — open issues with no covering open PR, excluding
                // human-gated / close-candidate (the producer's biggest, previously-hidden inbox).
                "uncoveredIssues": backlog.len(),
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
    println!(
        "▓▓ Producer backlog — untouched issues, no open PR (excl. human-gated / close-candidate): {}",
        backlog.len()
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
    /// This is an audit-lens checkout (`vet-<repo>-<n>`), created by `pr_checkout` and holding no
    /// work by construction — as opposed to a producer work clone, where an open PR means the work
    /// is live.
    vet: bool,
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

/// How long an audit-lens checkout may sit idle before the sweep reclaims it, INDEPENDENT of the
/// caller's `max_age_days` (which is the backstop for ad-hoc clones nobody modeled).
///
/// One day is ~12x the vetter's own `REVIEW_MAXTIME` ceiling of 2h, so a clone this idle cannot
/// belong to a run in flight, while the daily sweep still reclaims every leaked checkout within two
/// passes.
const VET_CLONE_MAX_AGE_DAYS: u64 = 1;

/// PURE: is this clone an audit-lens checkout? The name is the signal because it is the ONE thing
/// `pr_checkout` controls end to end ([`checkout_dir`]) and it survives a run that died before
/// releasing — which is precisely the clone that leaks.
fn is_vet_checkout(name: &str) -> bool {
    name.starts_with(VET_CLONE_PREFIX)
}

/// Decide whether a clone is safe to garbage-collect, with a reason. Precedence is deliberate:
/// unpushed/uncommitted work is ALWAYS preserved (never gc'd, whatever the PR state); then an
/// audit-lens checkout is disposable on age alone; then a merged/closed PR means the work has landed
/// or been abandoned upstream, so the clone is disposable; an open PR is active work (kept); a clone
/// with no resolvable PR is kept until it goes stale (the age backstop) so ad-hoc clones with no PR
/// don't accumulate forever.
///
/// The `vet` arm is #81. `pr_checkout` clones the PR the vetter is JUDGING, which is by definition an
/// OPEN PR, so the "open PR → active work" rule made every leaked audit checkout immortal: 83 of
/// them, 349 MB, none reclaimable by a sweep that was running nightly the whole time. A `vet-*` clone
/// is not work — it is a read-only copy of a commit that is already on GitHub, reproducible in
/// seconds, and its lifetime is ONE vetter run. The dirt/unpushed guards above still apply first, so
/// "never delete something that holds work" survives intact.
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
    if s.vet {
        return if s.age_days >= VET_CLONE_MAX_AGE_DAYS {
            GcAction::Delete(format!("vet checkout, idle {}d", s.age_days))
        } else {
            GcAction::Keep(format!(
                "vet checkout, idle {}d < {VET_CLONE_MAX_AGE_DAYS}d",
                s.age_days
            ))
        };
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

/// Days since anything last happened in this clone — the NEWER of the directory's own mtime and
/// `.git/HEAD`'s (0 on any error — errs toward KEEPING, since only the age backstops consult it).
///
/// The directory's mtime alone answers the wrong question. It changes when a TOP-LEVEL entry is
/// added or removed, and a checkout usually only rewrites files further down, so a clone that was
/// checked out ten minutes ago can read as days idle. That is a live hazard with the vet cap: a
/// vetter run spanning midnight, REUSING yesterday's checkout, would have had its working tree
/// deleted underneath it by the sweep. `.git/HEAD` is rewritten by every `git checkout` — including
/// the no-op `checkout -f -B` onto the branch already current — and, unlike `.git/index`, is NOT
/// touched by the `git status` the sweep itself runs, so it cannot make every clone immortal.
fn clone_age_days(dir: &std::path::Path) -> u64 {
    let mtime = |p: std::path::PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest = [mtime(dir.to_path_buf()), mtime(dir.join(".git/HEAD"))]
        .into_iter()
        .flatten()
        .max();
    newest
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// What the sweep did to (or would do to) one clone. `bytes` is the on-disk size, measured only for
/// a clone the sweep is deleting — walking every kept clone would cost a full stat of the whole
/// work-dir for a number nobody acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GcRecord {
    root: String,
    name: String,
    /// `deleted` | `would-delete` | `kept` | `error`
    outcome: &'static str,
    reason: String,
    bytes: u64,
}

/// Recursive on-disk size in bytes, symlinks counted as their own (tiny) entry and NEVER followed —
/// a symlink into `/nix/store` must not be reported as if the store lived inside the clone.
/// Unreadable entries contribute 0 rather than aborting the walk: this is a reporting figure.
fn dir_size_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue }; // read_dir metadata does NOT follow symlinks
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

/// Sweep ONE root: decide and (unless `dry_run`) act on every work clone directly under it,
/// reporting each decision to `on` as it is made. Errors only when the root itself is unreadable —
/// an individual clone's failure is a record, not an abort.
fn gc_clones_sweep(
    work_dir: &str,
    max_age_days: u64,
    dry_run: bool,
    on: &mut dyn FnMut(&GcRecord),
) -> Result<Vec<GcRecord>, String> {
    let entries =
        std::fs::read_dir(work_dir).map_err(|e| format!("cannot read work-dir {work_dir}: {e}"))?;
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(".git").is_dir())
        .collect();
    dirs.sort();
    let mut out = Vec::with_capacity(dirs.len());
    for dir in &dirs {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        // FULL status (untracked INCLUDED): an untracked file could be real uncommitted WIP, so gc
        // must keep a clone with ANY dirt — never ignore untracked to reclaim more. Cleanliness is
        // the PRODUCER's job (commit real work, gitignore ephemeral artifacts, keep temp files OUT of
        // the clone, then release the clone after submit) and the VETTER's gate (reject a PR whose
        // checkout goes dirty), NOT gc's to guess. A dirty clone left here = a hygiene bug upstream.
        // Same reading as `clone_release`'s, from ONE function: the unattended sweep and the attended
        // release must never disagree about whether a clone still holds work.
        let local = local_clone_state(dir);
        let clean = local.dirt.as_deref().map(|d| d.is_empty()).unwrap_or(false);
        let unpushed = local.unpushed;
        let vet = is_vet_checkout(&name);
        // Only pay for the `gh pr list` network round-trip once the clone is otherwise deletable: a
        // dirty or unpushed clone is KEPT regardless of its PR state, so skipping the call for it is
        // what keeps a full pass over hundreds of clones from dragging past any timeout. A `vet-*`
        // checkout skips it too — `gc_decision` never consults `pr` for one, so the call would buy
        // nothing but latency, once per leaked checkout.
        let pr = if clean && !vet && matches!(unpushed, Some(0)) {
            resolve_pr_state(dir)
        } else {
            None
        };
        let state = CloneState {
            clean,
            unpushed,
            pr,
            age_days: clone_age_days(dir),
            vet,
        };
        let rec = match gc_decision(&state, max_age_days) {
            GcAction::Delete(reason) => {
                let bytes = dir_size_bytes(dir);
                if dry_run {
                    GcRecord {
                        root: work_dir.to_string(),
                        name,
                        outcome: "would-delete",
                        reason,
                        bytes,
                    }
                } else {
                    match std::fs::remove_dir_all(dir) {
                        Ok(()) => GcRecord {
                            root: work_dir.to_string(),
                            name,
                            outcome: "deleted",
                            reason,
                            bytes,
                        },
                        Err(e) => GcRecord {
                            root: work_dir.to_string(),
                            name,
                            outcome: "error",
                            reason: format!("{reason}, but delete failed: {e}"),
                            bytes: 0,
                        },
                    }
                }
            }
            GcAction::Keep(reason) => GcRecord {
                root: work_dir.to_string(),
                name,
                outcome: "kept",
                reason,
                bytes: 0,
            },
        };
        on(&rec);
        out.push(rec);
    }
    Ok(out)
}

/// PURE: human-readable size. Reported next to every reclaim figure so "what would this free" is
/// answerable from the sweep's own output instead of a separate `du`.
fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `gc-clones <work-dir>... [--dry-run] [--max-age-days N]`: garbage-collect the per-PR/issue work
/// clones directly under EACH <work-dir>. A clone is deleted only when it is clean + fully pushed AND
/// its checked-out branch's PR is merged/closed (or it has no PR and has been idle past the age cap);
/// clones with uncommitted/unpushed work or an open PR are always kept. Prints one line per clone.
///
/// Several roots are accepted because clones do not all land in `WORK_DIR`: `review-run.sh` did not
/// substitute `{{WORK_DIR}}` into the vetter prompt, so `vet-*` clones accumulated in the INSTALL
/// dir, which a single-root sweep never looked at.
fn gc_clones_mode(work_dirs: &[String], max_age_days: u64, dry_run: bool) -> i32 {
    let (mut deleted, mut kept, mut scanned, mut freed) = (0u32, 0u32, 0usize, 0u64);
    let mut rc = 0;
    for work_dir in work_dirs {
        if work_dirs.len() > 1 {
            println!("== {work_dir} ==");
        }
        // Stream each decision immediately: on a full disk the deletes free space AS WE GO, and
        // progress stays visible so a long run never looks hung or gets cut off mid-scan.
        let mut print = |r: &GcRecord| {
            let label = match r.outcome {
                "deleted" => "deleted      ",
                "would-delete" => "would delete ",
                "kept" => "kept         ",
                _ => "ERROR        ",
            };
            let size = if r.bytes > 0 {
                format!(" [{}]", human_bytes(r.bytes))
            } else {
                String::new()
            };
            println!("{label} {}  ({}){size}", r.name, r.reason);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        };
        match gc_clones_sweep(work_dir, max_age_days, dry_run, &mut print) {
            Err(e) => {
                eprintln!("error: {e}");
                rc = 2;
            }
            Ok(recs) => {
                scanned += recs.len();
                for r in &recs {
                    match r.outcome {
                        "deleted" | "would-delete" => {
                            deleted += 1;
                            freed += r.bytes;
                        }
                        _ => kept += 1,
                    }
                }
            }
        }
    }
    let verb = if dry_run { "would gc" } else { "gc" };
    println!(
        "{verb}: {deleted} deleted, {kept} kept ({scanned} clones, {} reclaimed)",
        human_bytes(freed)
    );
    rc
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
    work_dirs: &[String],
    max_age_days: u64,
    dry_run: bool,
    do_clones: bool,
    do_nix: bool,
    nix_threshold: u8,
) -> i32 {
    let mut rc = 0;
    if do_clones {
        println!("== work clones ==");
        let c = gc_clones_mode(work_dirs, max_age_days, dry_run);
        if c != 0 {
            rc = c;
        }
    }
    if do_nix {
        let path = match work_dirs.first() {
            Some(d) if !d.is_empty() => d.as_str(),
            _ => "/nix/store",
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

// ─── work-clone lifecycle: a GUARDED filesystem surface, not a shell command ─────────────────────
//
// Creating and destroying work clones used to be the model composing shell: `git clone`, then
// `rm -rf <clonedir>` the moment the work was pushed (campaign-prompt step 6c). That delete was
// never actually possible — `campaign-settings.json` denies `Bash(rm -rf /:*)` and deny rules are
// PREFIX-matched, so it also matched `rm -rf /home/gildlab/code/<anything>`, i.e. every work-clone
// path (#56). The instruction and the permission rule contradicted each other for months and the
// box filled up.
//
// Widening the deny rule would fix that instance and leave the shape of the problem: an unbounded
// `rm -rf` whose safety is a string-matching rule. Here the delete is a TOOL instead. The model
// names a CLONE; it never supplies a path to remove. Every name is resolved through the guards
// below before any syscall, so "delete something outside the work roots" is not expressible.

/// The roots a work clone may live directly under, in preference order (`clone_create` always builds
/// in the first). `WORK_DIR` is where both runners put clones. `INSTALL_DIR` is the cron's own
/// checkout, included because it accumulated `vet-*` clones for months: `review-run.sh` did not
/// substitute `{{WORK_DIR}}` into the vetter prompt, so the vetter improvised its checkout path and
/// landed in cwd. Roots come from the ENVIRONMENT and never from a tool argument — a model-supplied
/// root would make every guard below vacuous.
fn clone_roots() -> Vec<String> {
    let mut roots = vec![vet_work_dir()];
    if let Ok(d) = std::env::var("INSTALL_DIR") {
        let d = d.trim_end_matches('/').to_string();
        if !d.is_empty() && !roots.contains(&d) {
            roots.push(d);
        }
    }
    roots
}

/// PURE: resolve `input` — a bare clone name, or an absolute path under `root` — to the single path
/// COMPONENT naming a work clone directly inside `root`. This is the whole path guard's first half,
/// and it runs before ANY filesystem call, so a refusal here cannot have an effect.
///
/// Refused: an absolute path outside `root` (including the sibling-prefix trick, `/x/codeEVIL` for
/// root `/x/code`), `..` in any position, a nested path, the root itself, `.`-prefixed entries
/// (`.git` is not a work clone), an embedded NUL, and the empty string.
fn clone_name_in_root(root: &str, input: &str) -> Result<String, String> {
    let root = root.trim_end_matches('/');
    if root.is_empty() || !root.starts_with('/') {
        return Err(format!(
            "work-clone root {root:?} is not an absolute path — refusing to resolve anything under it"
        ));
    }
    let bad = |why: &str| {
        format!("refusing {input:?}: {why}. Name one work clone directly under {root} (e.g. \"raindex-2444\")")
    };
    let s = input.trim();
    if s.is_empty() {
        return Err(bad("empty name"));
    }
    if s.contains('\0') {
        return Err(bad("embedded NUL"));
    }
    // `..` ANYWHERE is refused up front, before any prefix arithmetic — so a traversal can never be
    // laundered through an otherwise root-prefixed path (`/x/code/../../etc`).
    if std::path::Path::new(s)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(bad("`..` traversal"));
    }
    let rest = if s.starts_with('/') {
        if s.trim_end_matches('/') == root {
            return Err(bad("that is the root itself, not a clone in it"));
        }
        s.strip_prefix(&format!("{root}/"))
            .ok_or_else(|| bad("absolute path outside the work-clone root"))?
    } else {
        s
    };
    // No second `rest.is_empty()` check here: the root-itself case is already decided above, before
    // the prefix is stripped, so nothing can reach this point with an empty remainder. A mutation
    // pass proved a check here was unreachable — dead code in a guard reads as protection that is
    // not actually protecting anything.
    let rest = rest.trim_end_matches('/');
    if rest.contains('/') {
        return Err(bad("not a direct child of the root"));
    }
    if rest.starts_with('.') {
        return Err(bad("`.`-prefixed entries are never work clones"));
    }
    Ok(rest.to_string())
}

/// PURE: the same resolution against SEVERAL roots — first root that accepts the name wins. The
/// error names every root, so a model that guessed the wrong one is told where clones actually live.
fn clone_in_roots(roots: &[String], input: &str) -> Result<(String, String), String> {
    let mut first_err = None;
    for root in roots {
        match clone_name_in_root(root, input) {
            Ok(name) => return Ok((root.trim_end_matches('/').to_string(), name)),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    Err(match first_err {
        Some(e) => format!("{e} (work-clone roots: {})", roots.join(", ")),
        None => "no work-clone root is configured (WORK_DIR is unset)".to_string(),
    })
}

/// The path guard's second half: the checks that genuinely need the filesystem. Given an
/// already-name-resolved (root, name), confirm the target is a real work clone we may touch —
/// it exists, is a DIRECTORY and not a symlink, canonicalises to a DIRECT CHILD of the canonical
/// root (so a symlinked component cannot smuggle the path elsewhere), and contains `.git`.
///
/// That last check is the one that makes a mistake cheap: only a git clone is ever deletable, so no
/// argument — however malformed — reaches ordinary data.
///
/// This is a SECOND layer: `clone_name_in_root` should already have rejected anything that is not a
/// plain component. It is written to hold on its own anyway (and tested that way, called directly
/// with names the first layer would never emit), so relaxing the first layer cannot silently make
/// the root or its ancestors deletable.
fn resolve_existing_clone(root: &str, name: &str) -> Result<std::path::PathBuf, String> {
    let root_real = std::fs::canonicalize(root)
        .map_err(|e| format!("work-clone root {root} is not readable: {e}"))?;
    let path = root_real.join(name);
    let md = std::fs::symlink_metadata(&path)
        .map_err(|_| format!("no work clone {name:?} under {root}"))?;
    if md.file_type().is_symlink() {
        return Err(format!(
            "refusing {name:?}: it is a SYMLINK, not a work clone — releasing it would act on whatever it points at"
        ));
    }
    if !md.is_dir() {
        return Err(format!("refusing {name:?}: not a directory"));
    }
    let real = std::fs::canonicalize(&path)
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    if real == root_real || real.parent() != Some(root_real.as_path()) {
        return Err(format!(
            "refusing {name:?}: it resolves to {} — outside {}",
            real.display(),
            root_real.display()
        ));
    }
    if !real.join(".git").exists() {
        return Err(format!(
            "refusing {name:?}: no .git — this tool only ever touches git work clones"
        ));
    }
    Ok(real)
}

/// A clone's local git state, as the release decision sees it. `unpushed: None` means git could not
/// answer, which is treated exactly like "possibly unpushed".
struct LocalCloneState {
    unpushed: Option<u32>,
    dirt: Option<String>,
    branch: String,
}

fn local_clone_state(path: &std::path::Path) -> LocalCloneState {
    // Unpushed commits = on HEAD but on NO remote-tracking branch. This works WITHOUT a configured
    // upstream (unlike `@{u}..HEAD`, which errors on an upstream-less branch); a git error stays
    // `None` (not 0) so the decision fails safe on a clone whose push-state is unknown.
    let unpushed = match git_out(path, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .and_then(|s| s.parse::<u32>().ok())
    {
        Some(n) => Some(n),
        // …with ONE exception. An UNBORN HEAD — a clone interrupted before its first checkout — also
        // makes `rev-list HEAD` fail, and that is not an unknown state: there are no commits at all,
        // so there is nothing that could be lost. Without this, a half-finished clone is immortal.
        None if git_out(path, &["rev-parse", "--git-dir"]).is_some()
            && git_out(path, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_none() =>
        {
            Some(0)
        }
        None => None,
    };
    LocalCloneState {
        unpushed,
        dirt: git_out(path, &["status", "--porcelain"]),
        branch: git_out(path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default(),
    }
}

/// PURE: may this clone be released, given its local state and whether the caller explicitly
/// accepted losing uncommitted changes? Split out from the filesystem so the whole refusal ladder is
/// unit-testable.
///
/// - Commits that exist ONLY here are unrecoverable, so they refuse UNCONDITIONALLY — there is no
///   flag, because a flag is a thing a model under time pressure sets.
/// - An unknown push state is treated as unpushed (the same fail-safe `gc_decision` uses).
/// - Uncommitted changes refuse too, but `discard_uncommitted` overrides: in practice this dirt is
///   build/tooling output (`Cargo.lock` churn, generated pointers) that the producer is told to
///   gitignore, and refusing it outright is what leaves the clone on disk forever.
fn release_decision(s: &LocalCloneState, discard_uncommitted: bool) -> Result<(), String> {
    match s.unpushed {
        None => {
            return Err(
                "push state could not be determined — refusing to release (no flag overrides this)"
                    .to_string(),
            )
        }
        Some(n) if n > 0 => {
            return Err(format!(
                "{n} commit(s) exist only in this clone — push them first. No flag overrides this: the work would be unrecoverable"
            ))
        }
        Some(_) => {}
    }
    let Some(dirt) = &s.dirt else {
        return Err(
            "`git status` failed — refusing to release a clone whose state is unknown".to_string(),
        );
    };
    if !dirt.is_empty() && !discard_uncommitted {
        let lines: Vec<&str> = dirt.lines().collect();
        let shown: Vec<&str> = lines.iter().take(10).copied().collect();
        return Err(format!(
            "{} uncommitted change(s) — commit and push them, or pass discard_uncommitted:true once you have confirmed they are build/tooling output:\n{}{}",
            lines.len(),
            shown.join("\n"),
            if lines.len() > shown.len() { "\n…" } else { "" }
        ));
    }
    Ok(())
}

/// Release ONE work clone: the tool that replaces `rm -rf <clonedir>`. Every guard above runs first;
/// the size is measured before the delete so the trace records what was actually reclaimed.
fn clone_release_exec(root: &str, name: &str, discard_uncommitted: bool) -> Result<Value, String> {
    let path = resolve_existing_clone(root, name)?;
    let state = local_clone_state(&path);
    release_decision(&state, discard_uncommitted)
        .map_err(|why| format!("refusing to release {name:?}: {why}"))?;
    let bytes = dir_size_bytes(&path);
    let discarded = state.dirt.as_deref().unwrap_or("").lines().count();
    std::fs::remove_dir_all(&path).map_err(|e| format!("could not release {name:?}: {e}"))?;
    Ok(serde_json::json!({
        "released": name,
        "dir": path.to_string_lossy(),
        "branch": state.branch,
        "bytes": bytes,
        "size": human_bytes(bytes),
        "discardedUncommitted": discarded,
    }))
}

/// Create (or re-sync) a work clone. Same guard as release for the NAME; the destination may not
/// exist yet, so existence is the one check that differs. A re-sync is `fetch` + `checkout -f -B` +
/// `clean -fdx` — campaign-prompt step 4's recipe, moved into the binary so it can carry a guard the
/// shell version could not: a clone holding UNPUSHED commits is never re-synced over.
fn clone_create_exec(
    root: &str,
    name: &str,
    slug: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<Value, String> {
    std::fs::create_dir_all(root)
        .map_err(|e| format!("cannot create work-clone root {root}: {e}"))?;
    let root_real = std::fs::canonicalize(root)
        .map_err(|e| format!("work-clone root {root} is not readable: {e}"))?;
    let path = root_real.join(name);
    let existed = std::fs::symlink_metadata(&path).is_ok();
    if existed {
        // Reuse the FULL guard: a pre-existing entry that is a symlink, a file, or not a clone is
        // refused rather than clobbered.
        let path = resolve_existing_clone(root, name)?;
        let state = local_clone_state(&path);
        if !matches!(state.unpushed, Some(0)) {
            return Err(format!(
                "refusing to re-sync {name:?}: it holds commits that are not on any remote (or its push state is unknown) — push them, or use a different clone name"
            ));
        }
        git_run(&path, &["fetch", "origin", "--prune"])?;
    } else {
        gh_quiet(
            None,
            &[
                "repo",
                "clone",
                slug,
                &path.to_string_lossy(),
                "--",
                "--no-single-branch",
            ],
        )?;
    }
    let base = match base {
        Some(b) => b.to_string(),
        None => default_branch_of(&path)?,
    };
    git_run(
        &path,
        &["checkout", "-f", "-B", branch, &format!("origin/{base}")],
    )?;
    git_run(&path, &["clean", "-fdx"])?;
    Ok(serde_json::json!({
        "dir": path.to_string_lossy(),
        "repo": slug,
        "branch": branch,
        "base": base,
        "resynced": existed,
        "note": "release it with clone_release the moment the work is on GitHub",
    }))
}

/// `git -C <dir> <args>` for effect, surfacing git's own stderr on failure. Distinct from `git_out`,
/// which swallows failures into `None` because its callers WANT a fail-safe unknown.
fn git_run(dir: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = err.lines().rev().take(5).collect();
    Err(format!(
        "git {} failed: {}",
        args.join(" "),
        tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
    ))
}

/// The clone's default branch, read from the remote HEAD it already fetched (no network round-trip,
/// no assumption that it is called `main`).
fn default_branch_of(dir: &std::path::Path) -> Result<String, String> {
    if let Some(s) = git_out(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(b) = s.rsplit('/').next() {
            if !b.is_empty() {
                return Ok(b.to_string());
            }
        }
    }
    // `--no-single-branch` clones set origin/HEAD; a clone that predates that may not have it.
    git_out(dir, &["remote", "set-head", "origin", "--auto"])
        .and_then(|_| {
            git_out(
                dir,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
        })
        .and_then(|s| s.rsplit('/').next().map(String::from))
        .filter(|b| !b.is_empty())
        .ok_or_else(|| "could not determine the repo's default branch — pass `base`".to_string())
}

/// Every work clone under every configured root, with the state that decides whether it is
/// releasable. Read-only: the answer to "what is on this box and who owns it".
fn clone_list_exec(roots: &[String]) -> Result<Value, String> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join(".git").is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("?");
            let state = local_clone_state(&dir);
            out.push(serde_json::json!({
                "name": name,
                "root": root,
                "branch": state.branch,
                "unpushed": state.unpushed,
                "uncommitted": state.dirt.as_deref().map(|d| d.lines().count()),
                "ageDays": clone_age_days(&dir),
                "releasable": release_decision(&state, false).is_ok(),
            }));
        }
    }
    Ok(serde_json::json!({"roots": roots, "clones": out}))
}

/// The unattended sweep, as a tool. Same decision function as the CLI (`gc_decision`), across every
/// configured root, returning what it did instead of printing it — STDOUT is the MCP protocol.
fn clone_gc_exec(roots: &[String], max_age_days: u64, dry_run: bool) -> Result<Value, String> {
    let mut recs = Vec::new();
    let mut errors = Vec::new();
    for root in roots {
        match gc_clones_sweep(root, max_age_days, dry_run, &mut |_r| {}) {
            Ok(mut r) => recs.append(&mut r),
            Err(e) => errors.push(e),
        }
    }
    let freed: u64 = recs.iter().map(|r| r.bytes).sum();
    let deleted = recs
        .iter()
        .filter(|r| r.outcome == "deleted" || r.outcome == "would-delete")
        .count();
    Ok(serde_json::json!({
        "dryRun": dry_run,
        "roots": roots,
        "scanned": recs.len(),
        "deleted": deleted,
        "kept": recs.len() - deleted,
        "bytesReclaimed": freed,
        "reclaimed": human_bytes(freed),
        "errors": errors,
        "clones": recs.iter().map(|r| serde_json::json!({
            "name": r.name, "root": r.root, "outcome": r.outcome,
            "reason": r.reason, "bytes": r.bytes,
        })).collect::<Vec<_>>(),
    }))
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

// ─────────────────────────────────────────────────────────────────────────────
// unvetted (the VETTER's state-load, #59) + the FSM MCP server (#52).
//
// `unvetted` is the vetter's state-load, the counterpart to the producer's `worklist` /
// `uncovered-issues`: which open PRs need a verdict this run, decided in-process — one call, one
// struct, no per-PR shelling into the model's context.
//
// The MCP server exposes exactly that state-load plus the vetter's other three moves as typed tools,
// and is the vetter's WHOLE surface: it runs with NO Bash at all, so a non-FSM operation is
// unrepresentable rather than merely forbidden by prose (Bash deny-lists are prefix-matched and
// bypassable). It also means the vetter never builds or executes anything in a `pr_checkout` clone —
// it reads source only; clean-tree and test-execution checks belong to the producer and CI. The surface is
// deliberately TINY — every tool schema rides in the preamble on every API call, so one wrapper per
// `gh` command would spend back what the prose removal saves.
// ─────────────────────────────────────────────────────────────────────────────

/// What the vetter must do with ONE open PR this run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VetAction {
    Vet,
    SkipHuman,
    SkipDraft,
    SkipVetted,
    SkipOpenThreads,
}

impl VetAction {
    fn as_str(self) -> &'static str {
        match self {
            VetAction::Vet => "vet",
            VetAction::SkipHuman => "skip-human-decided",
            VetAction::SkipDraft => "skip-draft",
            VetAction::SkipVetted => "skip-vetted-at-head",
            VetAction::SkipOpenThreads => "skip-open-threads",
        }
    }
}

/// PURE vet-lifecycle transition guard. **THE ORDER IS THE GUARD**: the human-sacred check resolves
/// BEFORE any head/vetted comparison, so a moved head can never reopen a human-decided PR (on
/// 2026-07-04 a run re-vetted human-rejected rain.erc4626.words#162 after a merge-main commit moved
/// its head — that exact sequence is what this ordering forbids). `human_sacred` covers BOTH forms of
/// human decision: a `human:*` label and a native `APPROVED`/`CHANGES_REQUESTED` review.
fn vet_action(is_draft: bool, human_sacred: bool, vetted_at_head: bool) -> VetAction {
    if human_sacred {
        return VetAction::SkipHuman;
    }
    if is_draft {
        return VetAction::SkipDraft;
    }
    if vetted_at_head {
        VetAction::SkipVetted
    } else {
        VetAction::Vet
    }
}

/// The open-threads gate on the VETTER's state-load, applied to one already-classified `unvetted`
/// row. It is the vetter's half of issue #1 — "do not record a `ready` verdict while the PR has
/// unresolved review comments" — implemented HERE rather than in `review-prompt.txt` because
/// #63/#64 removed the vetter's Bash: it has no `gh`, so it cannot run the reviewThreads query
/// itself. The exclusion has to happen in the tool that hands it the list.
///
/// `fetch` is called ONLY for a row that would otherwise be VETTED — every other action already
/// skips the PR, so it must not pay a GraphQL round-trip to learn something that changes nothing.
///
/// FAIL-CLOSED, matching `--queue`: an unreadable thread state (`None`) is NOT offered for vetting,
/// so a transient API failure can never launder a thread-dirty PR into an `ai:ready` verdict. The
/// cost is one deferred vet — the next run re-reads it — against a wrong `ready` label that then
/// needs a human to unwind. The count rides on the row as `unresolvedThreads` (`null` = unreadable)
/// so an operator reading `--json` can tell "dirty" from "unknown".
fn gate_open_threads(
    row: (VetAction, u8, Value),
    fetch: impl FnOnce() -> Option<u64>,
) -> (VetAction, u8, Value) {
    let (action, prio, mut json) = row;
    if action != VetAction::Vet {
        return (action, prio, json);
    }
    let threads = fetch();
    let Some(obj) = json.as_object_mut() else {
        return (action, prio, json);
    };
    obj.insert(
        "unresolvedThreads".into(),
        threads.map(Value::from).unwrap_or(Value::Null),
    );
    // The vetter collapses `OpenThreads` and `FetchError` into ONE skip: in both cases the thread
    // state is not verified clean, and the vetter's handling is identical (don't vet it this run).
    // The row's `unresolvedThreads` (`null` vs a number) keeps the two distinguishable to a reader.
    if thread_route(threads) == ThreadRoute::Present {
        return (action, prio, json);
    }
    obj.insert(
        "action".into(),
        Value::from(VetAction::SkipOpenThreads.as_str()),
    );
    (VetAction::SkipOpenThreads, prio, json)
}

/// PURE: vet-first ordering — lower sorts first. The prompt's "vet green+mergeable ones first" rule,
/// computed here instead of costing a `gh pr view` per PR inside the model loop. CI/mergeability is
/// NEVER a reason to withhold a verdict (that stays a prompt-level judgement rule); it only decides
/// which un-vetted PR is closest to merge and therefore worth vetting first.
fn vet_priority(ci: Ci, merge: Merge) -> u8 {
    match (ci, merge) {
        (Ci::Green, Merge::Mergeable) => 0,
        (Ci::NoChecks, Merge::Mergeable) => 1,
        (Ci::Green | Ci::NoChecks, _) => 2,
        (Ci::Pending, _) => 3,
        (Ci::Red, _) => 4,
    }
}

fn merge_str(m: Merge) -> &'static str {
    match m {
        Merge::Mergeable => "MERGEABLE",
        Merge::Conflicting => "CONFLICTING",
        Merge::Unknown => "UNKNOWN",
    }
}

fn parse_merge(v: Option<&str>) -> Merge {
    match v {
        Some("MERGEABLE") => Merge::Mergeable,
        Some("CONFLICTING") => Merge::Conflicting,
        _ => Merge::Unknown,
    }
}

fn label_names(v: &Value) -> Vec<String> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// PURE: one candidate's `unvetted` row + its action + its vet-first sort key, derived from the PR
/// detail JSON. Everything issue #59 asks for per candidate — `headRefOid`, `labels`,
/// `reviewDecision`, human-sacred flag, vetted-at-head flag, `ci`, `mergeable` — in one place.
fn unvetted_row(
    slug: &str,
    num: u64,
    url: &str,
    title: &str,
    detail: &Value,
) -> (VetAction, u8, Value) {
    let head = detail
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let human_sacred = has_human_override(detail) || has_native_human_review(detail);
    let vetted = vetted_at_head(detail, head);
    let is_draft = detail
        .get("isDraft")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let ci = classify_ci(detail.get("statusCheckRollup").unwrap_or(&Value::Null));
    let merge = parse_merge(detail.get("mergeable").and_then(|v| v.as_str()));
    let action = vet_action(is_draft, human_sacred, vetted);
    let review_decision = detail
        .get("reviewDecision")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let row = serde_json::json!({
        "pr": format!("{slug}#{num}"),
        "url": url,
        "title": title,
        "headRefOid": head,
        "labels": label_names(detail),
        "reviewDecision": review_decision,
        "humanSacred": human_sacred,
        "vettedAtHead": vetted,
        "ci": ci_str(ci),
        "mergeable": merge_str(merge),
        "isDraft": is_draft,
        "action": action.as_str(),
    });
    (action, vet_priority(ci, merge), row)
}

/// PURE: take at most `limit` items off the front of `items`, returning (page, not-on-this-page).
/// `None` means unbounded — the CLI/dashboard shape, where the consumer is a shell or an in-process
/// caller with no token budget.
fn page<T>(items: Vec<T>, limit: Option<usize>) -> (Vec<T>, usize) {
    let Some(limit) = limit else {
        return (items, 0);
    };
    let more = items.len().saturating_sub(limit);
    let mut items = items;
    items.truncate(limit);
    (items, more)
}

/// PURE: the tiny projection of a skipped row that is worth spending a caller's context on — which
/// PR, why it was withheld, and (for the open-threads gate) how many threads. The full row carries
/// `headRefOid`/`labels`/`ci`/`mergeable`/`reviewDecision`, all of which describe a PR the caller is
/// about to do NOTHING with; 150 of them is what made the state-load unreadable (#78).
fn skipped_digest(row: &Value) -> Value {
    let mut d = serde_json::json!({
        "pr": row.get("pr").cloned().unwrap_or(Value::Null),
        "action": row.get("action").cloned().unwrap_or(Value::Null),
    });
    if let Some(t) = row.get("unresolvedThreads") {
        d.as_object_mut()
            .expect("object")
            .insert("unresolvedThreads".into(), t.clone());
    }
    d
}

/// PURE: the `unvetted` document from classified rows. `prs` holds the PRs to VET, in vet-first order
/// (priority, then a stable pr key), capped at `limit` with the remainder reported as `more`.
///
/// BOUNDED BY CONSTRUCTION (#78). Every list here is a PAGE, not a dump: on 2026-07-27 this document
/// returned 63,742 characters on ONE line, the vetter's harness refused it as over-budget, and the
/// vetter silently re-called without `include_skipped` — dropping the whole open-threads accounting
/// #2 had landed an hour earlier. Paging is the fix that does not depend on the queue staying small:
/// the vetter vets ONE PR at a time and each verdict removes its PR from the next call's page, so a
/// page walks the queue without an offset argument.
///
/// `openThreads` is UNCONDITIONAL and is the reason `include_skipped` is no longer load-bearing: the
/// PRs withheld for unresolved threads are the only skipped rows carrying per-row information the
/// vetter can act on (a PR left the queue with no verdict, and `unresolvedThreads` says why). Making
/// it depend on an optional argument is exactly how that accounting went missing.
fn unvetted_doc(
    rows: &[(VetAction, u8, Value)],
    include_skipped: bool,
    limit: Option<usize>,
) -> Value {
    let mut vet: Vec<(u8, String, Value)> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut open_threads: Vec<Value> = Vec::new();
    let (mut n_draft, mut n_human, mut n_vetted, mut n_threads) = (0usize, 0usize, 0usize, 0usize);
    for (action, prio, row) in rows {
        match action {
            VetAction::Vet => {
                let key = row
                    .get("pr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                vet.push((*prio, key, row.clone()));
            }
            other => {
                // Exhaustive on purpose: a new `VetAction` must be given its own count rather than
                // silently folding into `skipVettedAtHead` (which is what a `_` arm did).
                match other {
                    VetAction::SkipDraft => n_draft += 1,
                    VetAction::SkipHuman => n_human += 1,
                    VetAction::SkipOpenThreads => {
                        n_threads += 1;
                        open_threads.push(serde_json::json!({
                            "pr": row.get("pr").cloned().unwrap_or(Value::Null),
                            "url": row.get("url").cloned().unwrap_or(Value::Null),
                            "unresolvedThreads": row
                                .get("unresolvedThreads")
                                .cloned()
                                .unwrap_or(Value::Null),
                        }));
                    }
                    // `Vet` is taken by the arm above; the only action left here is vetted-at-head.
                    VetAction::SkipVetted | VetAction::Vet => n_vetted += 1,
                }
                skipped.push(row.clone());
            }
        }
    }
    vet.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let n_vet = vet.len();
    let (page_rows, more) = page(vet, limit);
    let prs: Vec<Value> = page_rows.into_iter().map(|(_, _, r)| r).collect();
    let (open_threads, more_threads) = page(open_threads, limit);
    let mut doc = serde_json::json!({
        "counts": {
            "open": rows.len(),
            "vet": n_vet,
            "skipDraft": n_draft,
            "skipHumanDecided": n_human,
            "skipVettedAtHead": n_vetted,
            "skipOpenThreads": n_threads,
        },
        "prs": prs,
        // How many vet-able PRs this page LEFT BEHIND. A caller that reads `prs.len()` as the whole
        // queue is wrong by exactly this number, so the number is stated rather than inferable.
        "more": more,
        "openThreads": open_threads,
        "moreOpenThreads": more_threads,
    });
    if include_skipped {
        let (rows, more_skipped) = page(skipped, limit);
        let obj = doc.as_object_mut().expect("object");
        obj.insert(
            "skipped".into(),
            Value::Array(rows.iter().map(skipped_digest).collect()),
        );
        obj.insert("moreSkipped".into(), Value::from(more_skipped));
    }
    doc
}

/// PURE: one row of the close-candidate vet queue, plus whether it is to be vetted and why not.
/// Split out so the skip reasons are unit-testable without the network — the same shape the PR-side
/// state-load uses.
fn cc_row(slug: &str, num: u64, title: &str, detail: &Value) -> (bool, &'static str, Value) {
    let labels = label_names(detail);
    let human = has_human_ruling(&labels);
    let flag = last_close_candidate_flag(detail);
    let flag_at = flag.as_ref().map(|(a, _)| a.clone()).unwrap_or_default();
    let vetted = cc_vetted_at_flag(detail, &flag_at);
    // Precedence mirrors the PR side: a human ruling dominates, then "nothing to judge", then a
    // verdict already recorded against THIS flag.
    let (vet, action) = if human {
        (false, "skip-human-decided")
    } else if flag.is_none() {
        (false, "skip-no-flag")
    } else if vetted {
        (false, "skip-vetted-at-flag")
    } else {
        (true, "vet")
    };
    (
        vet,
        action,
        serde_json::json!({
            "issue": format!("{slug}#{num}"),
            // `repo`/`number`/`title` mirror the item shape `closeCandidateIssues` and
            // `uncoveredIssues` already emit, so the dashboard reads every issue array
            // generically — no special-casing for the split states.
            "repo": slug,
            "number": num,
            "url": detail.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            "title": title,
            "labels": labels,
            "humanSacred": human,
            "flagAt": flag_at,
            "flagReason": flag.as_ref().map(|(_, b)| flag_reason(b)).unwrap_or_default(),
            "vettedAtFlag": vetted,
            "action": action,
        }),
    )
}

/// PURE: split an `unvetted_close_candidates` document into the two dashboard ITEM ARRAYS —
/// `(unvetted, upheld)` — in the same `{repo, number, title}` shape `closeCandidateIssues` and
/// `uncoveredIssues` already emit.
///
/// Both the arrays and their counts are derived from THIS one document, which is what makes an
/// array/count mismatch unrepresentable: a box that renders "5" and then lists three issues when
/// clicked is the drift this shape rules out.
///
/// `upheld` is the skipped rows whose action is `skip-vetted-at-flag`: a REJECTED flag has its
/// label stripped, so it cannot appear in this search at all — an issue still carrying the label
/// AND vetted at its current flag was necessarily upheld.
fn cc_item_arrays(doc: &Value) -> (Vec<Value>, Vec<Value>) {
    let item = |r: &Value| {
        serde_json::json!({
            "repo": r.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
            "number": r.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
            "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        })
    };
    let unvetted: Vec<Value> = doc
        .get("issues")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(item).collect())
        .unwrap_or_default();
    let upheld: Vec<Value> = doc
        .get("skipped")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| r.get("action").and_then(|v| v.as_str()) == Some("skip-vetted-at-flag"))
                .map(item)
                .collect()
        })
        .unwrap_or_default();
    (unvetted, upheld)
}

/// PURE: an issue state's dashboard pair — the ITEM ARRAY and the count that labels it, from ONE
/// source.
///
/// The count is not passed in and cannot be computed separately: emitting `items.len()` at the
/// array site and again at the counts site let the two drift, and a mutation decoupling them
/// survived (a box rendering "5" that lists three issues when clicked). Deriving both here makes
/// that unrepresentable, and puts the invariant somewhere a test can actually reach — the emission
/// site itself is inside a network call.
fn issue_state_pair(items: Vec<Value>) -> (Value, usize) {
    let n = items.len();
    (Value::Array(items), n)
}

/// PURE: the `Close-candidate: <category>: <evidence>` payload of a flag body, without the marker
/// line — the claim the vetter has to judge.
fn flag_reason(body: &str) -> String {
    body.lines()
        .find_map(|l| l.trim().strip_prefix("Close-candidate:"))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Live `unvetted-close-candidates` state-load: ONE org-wide search for open `ai:close-candidate`
/// issues + one `gh issue view` each. Errors rather than returning a falsely-empty set, for the same
/// reason the PR side does — an empty queue must never be an API failure in disguise.
///
/// `limit` pages the `issues` list for the same reason [`unvetted_doc`] pages `prs` (#78): the MCP
/// caller has a token budget, so it always supplies one. The DASHBOARD passes `None` — it derives
/// `closeCandidateUnvetted`/`closeCandidateUpheld` from this document's own arrays, and a paged array
/// would render a count that disagrees with the list under it.
fn unvetted_close_candidates_fetch(
    include_skipped: bool,
    limit: Option<usize>,
) -> Result<Value, String> {
    let mut args: Vec<String> = vec!["search".into(), "issues".into()];
    args.extend(org_owner_args());
    args.extend(
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
    let argref: Vec<&str> = args.iter().map(String::as_str).collect();
    let found = gh_json(&argref)
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| {
            "error: `gh search issues --label ai:close-candidate` failed (transient API/auth?) — \
             aborting rather than report a falsely-empty close-candidate queue"
                .to_string()
        })?;
    // The dashboard's `closeCandidateUnvetted` reads this queue, so a per-issue failure must be
    // reported (see `fetchErrors` below), never dropped.

    let mut rows: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    let (mut n_human, mut n_noflag, mut n_vetted) = (0usize, 0usize, 0usize);
    for i in &found {
        let url = i.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let title = i.get("title").and_then(|t| t.as_str()).unwrap_or("");
        // `repository.nameWithOwner` is authoritative; the url parse is the fallback.
        let slug = i
            .get("repository")
            .and_then(|r| r.get("nameWithOwner"))
            .and_then(|s| s.as_str())
            .map(String::from)
            .or_else(|| issue_slug(url));
        let (Some(slug), Some(num)) = (slug, i.get("number").and_then(|n| n.as_u64())) else {
            errors.push(
                serde_json::json!({"url": url, "title": title, "error": "unparseable issue ref"}),
            );
            continue;
        };
        // A dropped issue must be VISIBLE. Silently `continue`ing shrinks the vetter's inbox and
        // makes `flagged` disagree with `vet + skip*` with nothing to explain the gap.
        let Some(detail) = gh_json(&[
            "issue",
            "view",
            &num.to_string(),
            "-R",
            &slug,
            "--json",
            "number,title,url,state,labels,comments",
        ]) else {
            errors.push(serde_json::json!({
                "issue": format!("{slug}#{num}"),
                "title": title,
                "error": "gh issue view failed — not judged this run",
            }));
            continue;
        };
        let (vet, action, row) = cc_row(&slug, num, title, &detail);
        if vet {
            rows.push(row);
        } else {
            match action {
                "skip-human-decided" => n_human += 1,
                "skip-no-flag" => n_noflag += 1,
                _ => n_vetted += 1,
            }
            skipped.push(row);
        }
    }

    let n_vet = rows.len();
    let (issues, more) = page(rows, limit);
    let mut doc = serde_json::json!({
        "counts": {
            "flagged": found.len(),
            "vet": n_vet,
            "skipHumanDecided": n_human,
            "skipNoFlag": n_noflag,
            "skipVettedAtFlag": n_vetted,
            // flagged == vet + skip* + fetchErrors, always. A non-zero value here is the ONLY
            // reason the parts may not sum to the whole.
            "fetchErrors": errors.len(),
        },
        "issues": issues,
        "more": more,
        "fetchErrors": errors,
    });
    if include_skipped {
        let (skipped, more_skipped) = page(skipped, limit);
        let obj = doc.as_object_mut().expect("object");
        obj.insert("skipped".into(), Value::Array(skipped));
        obj.insert("moreSkipped".into(), Value::from(more_skipped));
    }
    Ok(doc)
}

/// PURE: the judging bundle for ONE flagged issue — the claim, and everything needed to test it.
/// The issue-side twin of [`pr_context_doc`]; comments are the TRUSTED ones only.
fn close_candidate_context_doc(slug: &str, num: u64, detail: &Value) -> Value {
    let flag = last_close_candidate_flag(detail);
    let flag_at = flag.as_ref().map(|(a, _)| a.clone()).unwrap_or_default();
    serde_json::json!({
        "issue": format!("{slug}#{num}"),
        "url": detail.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "title": detail.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "body": detail.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "state": detail.get("state").and_then(|v| v.as_str()).unwrap_or(""),
        // The issue's OWN creation time is the recency baseline: evidence dated before this cannot
        // be the fix, which is failure class 1.
        "createdAt": detail.get("createdAt").and_then(|v| v.as_str()).unwrap_or(""),
        "labels": label_names(detail),
        "humanSacred": has_human_ruling(&label_names(detail)),
        "flagAt": flag_at,
        "flagReason": flag.as_ref().map(|(_, b)| flag_reason(b)).unwrap_or_default(),
        "flagBody": flag.as_ref().map(|(_, b)| b.clone()).unwrap_or_default(),
        "vettedAtFlag": cc_vetted_at_flag(detail, &flag_at),
        "producerComments": trusted_comments(detail, Some("🤖 ai:producer")),
        "vetterComments": trusted_comments(detail, Some("🤖 ai:vetter")),
    })
}

/// Live `close_candidate_context`.
fn close_candidate_context_fetch(slug: &str, num: u64) -> Result<Value, String> {
    let detail = gh_json(&[
        "issue",
        "view",
        &num.to_string(),
        "-R",
        slug,
        "--json",
        "number,title,body,url,state,createdAt,labels,comments",
    ])
    .ok_or_else(|| format!("error: `gh issue view {slug}#{num}` failed"))?;
    Ok(close_candidate_context_doc(slug, num, &detail))
}

/// The close-candidate verdict write — the issue-side twin of [`record_verdict_apply`]. `uphold`
/// leaves the flag queued for the human; `reject` REMOVES `ai:close-candidate`, which returns the
/// issue to the producer's uncovered-issues queue. Either way a trusted, flag-pinned `🤖 ai:vetter`
/// comment records the judgement.
///
/// The vetter never writes a `human:*` label: those are the human's namespace, and this routine is
/// AI. Its whole authority is the `ai:*` flag it may drop and the comment it may post.
fn record_cc_verdict_apply(
    slug: &str,
    issue: &str,
    verdict: &str,
    note: &str,
    dry_run: bool,
) -> Result<String, (i32, String)> {
    if !CC_VERDICTS.contains(&verdict) {
        return Err((
            2,
            format!(
                "{verdict:?} is not a close-candidate verdict — use one of: {}",
                CC_VERDICTS.join(", ")
            ),
        ));
    }
    if note.trim().is_empty() {
        return Err((
            2,
            "note is required: one line naming what the evidence proves or fails to prove"
                .to_string(),
        ));
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
        return Err((
            1,
            format!(
                "error: `gh issue view {slug}#{issue}` failed — not writing on incomplete data"
            ),
        ));
    };
    let (flag_at, remove_label, skip) = match cc_verdict_plan(&j, verdict) {
        CcVerdictPlan::AlreadyClosed => {
            return Ok(format!("{slug}#{issue} already closed — nothing to vet"));
        }
        CcVerdictPlan::RefuseHuman => {
            return Err((
                3,
                format!("human ruling present on {slug}#{issue}; not overriding"),
            ));
        }
        CcVerdictPlan::NoFlag => {
            return Err((
                4,
                format!(
                    "no trusted producer close-candidate flag on {slug}#{issue} — nothing to judge"
                ),
            ));
        }
        CcVerdictPlan::Record {
            flag_at,
            remove_label,
            skip_comment,
        } => (flag_at, remove_label, skip_comment),
    };
    let comment = cc_verdict_comment(&flag_at, verdict, note);

    if dry_run {
        return Ok(format!(
            "[dry-run] {slug}#{issue} flag @ {flag_at}\n  verdict: {verdict}\n  label: {}\n  comment: {}",
            if remove_label {
                "remove ai:close-candidate"
            } else {
                "unchanged"
            },
            if skip {
                "skip (same verdict at same flag already posted)".to_string()
            } else {
                format!("post -> {}", comment.replace('\n', " / "))
            }
        ));
    }

    // Comment BEFORE the label drop: a rejected flag whose label vanished with no recorded reason
    // is indistinguishable from a human de-flagging it by hand.
    if !skip && !gh_run(&["issue", "comment", issue, "-R", slug, "--body", &comment]) {
        return Err((
            1,
            format!("error: failed to post the close-candidate verdict comment on {slug}#{issue}"),
        ));
    }
    if remove_label
        && !gh_run(&[
            "issue",
            "edit",
            issue,
            "-R",
            slug,
            "--remove-label",
            "ai:close-candidate",
        ])
    {
        return Err((
            1,
            format!(
                "error: posted the verdict on {slug}#{issue} but FAILED to remove ai:close-candidate"
            ),
        ));
    }
    Ok(format!(
        "recorded close-candidate {verdict} on {slug}#{issue} @ {flag_at}{}{}",
        if remove_label {
            " [ai:close-candidate removed -> back to the producer queue]"
        } else {
            " [flag stands -> queued for the human]"
        },
        if skip { " [comment deduped]" } else { "" }
    ))
}

/// Live `unvetted` state-load: ONE org-wide search + one `gh pr view` per open non-draft PR whose
/// labels don't already carry a human decision. Errors (rather than returning a falsely-empty set) if
/// the search fails — an empty vet queue must never be an API failure in disguise.
///
/// `limit` bounds the returned PAGE, not the work: every open PR is still classified (the counts are
/// whole-queue), only the listed rows are capped. `None` = unbounded, the CLI shape.
fn unvetted_fetch(include_skipped: bool, limit: Option<usize>) -> Result<Value, String> {
    let assignee = pr_assignee();
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
            "url,number,repository,title,isDraft,labels",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let argref: Vec<&str> = args.iter().map(String::as_str).collect();
    let prs = gh_json(&argref)
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| {
            format!("error: `gh search prs --author {assignee}` failed (transient API/auth?) — aborting rather than report a falsely-empty vet queue")
        })?;

    let mut rows: Vec<(VetAction, u8, Value)> = Vec::new();
    for p in &prs {
        let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let (Some(slug), Some(num)) = (pr_slug(url), p.get("number").and_then(|n| n.as_u64()))
        else {
            continue;
        };
        let title = p.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let is_draft = p.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);
        // Cheap pre-filter: a draft or an already-human-decided PR is skipped straight from the
        // search JSON — no per-PR fetch. (A native human REVIEW is invisible to search, so every
        // remaining PR is still fetched and re-checked below, human-first.)
        if is_draft || has_human_override(p) {
            let action = vet_action(is_draft, has_human_override(p), false);
            rows.push((
                action,
                4,
                serde_json::json!({
                    "pr": format!("{slug}#{num}"),
                    "url": url,
                    "title": title,
                    "labels": label_names(p),
                    "isDraft": is_draft,
                    "action": action.as_str(),
                }),
            ));
            continue;
        }
        let Some(detail) = gh_json(&[
            "pr",
            "view",
            &num.to_string(),
            "-R",
            &slug,
            "--json",
            "headRefOid,labels,reviewDecision,mergeable,statusCheckRollup,comments,isDraft",
        ]) else {
            return Err(format!(
                "error: `gh pr view {slug}#{num}` failed — aborting rather than report an incomplete vet queue"
            ));
        };
        // Classify first, THEN gate on open threads — the gate's `fetch` runs only for a row that
        // would actually be vetted, so an already-skipped PR costs no extra GraphQL round-trip.
        // An unsplittable slug yields None (fail-closed: not vetted this run), never a dropped PR.
        rows.push(gate_open_threads(
            unvetted_row(&slug, num, url, title, &detail),
            || {
                let (owner, repo) = slug.split_once('/')?;
                unresolved_threads(owner, repo, num)
            },
        ));
    }
    Ok(unvetted_doc(&rows, include_skipped, limit))
}

fn unvetted_mode(json_out: bool, include_skipped: bool, limit: Option<usize>) -> i32 {
    let doc = match unvetted_fetch(include_skipped, limit) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if json_out {
        println!("{doc}");
        return 0;
    }
    let c = &doc["counts"];
    println!(
        "un-vetted: {} to vet ({} open · {} draft · {} human-decided · {} vetted-at-head · {} open-threads)",
        c["vet"],
        c["open"],
        c["skipDraft"],
        c["skipHumanDecided"],
        c["skipVettedAtHead"],
        c["skipOpenThreads"]
    );
    for p in doc["prs"].as_array().into_iter().flatten() {
        println!(
            "  {}  [{} · {}]  {}",
            p["pr"].as_str().unwrap_or(""),
            p["ci"].as_str().unwrap_or("?"),
            p["mergeable"].as_str().unwrap_or("?"),
            p["title"].as_str().unwrap_or("")
        );
    }
    if doc["more"].as_u64().unwrap_or(0) > 0 {
        println!("  … {} more not on this page", doc["more"]);
    }
    // The PRs withheld because a review thread is still open. Printed unconditionally: this is the
    // one skip reason that says a PR left the queue with NO verdict and work is owed on it.
    for t in doc["openThreads"].as_array().into_iter().flatten() {
        println!(
            "  {}  [withheld · {} unresolved thread(s)]",
            t["pr"].as_str().unwrap_or(""),
            t["unresolvedThreads"]
        );
    }
    0
}

/// Default cap on the diff a single `pr_context` returns. A diff is the vetter's biggest single read;
/// past this the model is reading a generated-artifact dump, not a reviewable change.
///
/// It is the whole result budget on purpose: the tool FITS the diff to what is left after the
/// metadata (see [`pr_context_fetch`]), so the default asks for "as much diff as can be delivered"
/// rather than for a number that has to be guessed and then refused.
const DEFAULT_MAX_DIFF_BYTES: usize = MCP_MAX_RESULT_BYTES;
/// Hard ceiling a caller may raise `max_diff_bytes` to. Asking for more diff than a whole result may
/// occupy is not expressible — that is what kept `pr_context`'s effective budget above the harness's
/// ceiling no matter what the guard said (#81).
const MAX_MAX_DIFF_BYTES: u64 = MCP_MAX_RESULT_BYTES as u64;

/// PURE: truncate to at most `max` BYTES on a char boundary (never panics on multi-byte input);
/// returns (text, truncated?).
fn truncate_utf8(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// PURE: the whole review bundle for ONE PR — what `gh pr view` + `gh pr diff` + an `gh issue view`
/// per linked issue used to cost inside the model's context, in a single document. Comments are the
/// TRUSTED ones only (author-verified, per the provenance invariant), so a spoofed `🤖 ai:vetter`
/// marker from a third party can never be read as a prior verdict.
fn pr_context_doc(
    slug: &str,
    num: u64,
    detail: &Value,
    diff: &str,
    issues: &[Value],
    max_diff_bytes: usize,
) -> Value {
    let head = detail
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let ci = classify_ci(detail.get("statusCheckRollup").unwrap_or(&Value::Null));
    let merge = parse_merge(detail.get("mergeable").and_then(|v| v.as_str()));
    let files: Vec<Value> = detail
        .get("files")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .map(|f| {
                    serde_json::json!({
                        "path": f.get("path").and_then(|p| p.as_str()).unwrap_or(""),
                        "additions": f.get("additions").and_then(|x| x.as_u64()).unwrap_or(0),
                        "deletions": f.get("deletions").and_then(|x| x.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let closes: Vec<u64> = detail
        .get("closingIssuesReferences")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("number").and_then(|n| n.as_u64()))
                .collect()
        })
        .unwrap_or_default();
    let (diff_text, truncated) = truncate_utf8(diff, max_diff_bytes);
    serde_json::json!({
        "pr": format!("{slug}#{num}"),
        "url": detail.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "title": detail.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "body": detail.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "headRefOid": head,
        "isDraft": detail.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false),
        "labels": label_names(detail),
        "reviewDecision": detail.get("reviewDecision").and_then(|v| v.as_str()).filter(|s| !s.is_empty()),
        "humanSacred": has_human_override(detail) || has_native_human_review(detail),
        "vettedAtHead": vetted_at_head(detail, head),
        "ci": ci_str(ci),
        "mergeable": merge_str(merge),
        "additions": detail.get("additions").and_then(|v| v.as_u64()).unwrap_or(0),
        "deletions": detail.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0),
        "files": files,
        "closes": closes,
        "issues": issues,
        "vetterComments": trusted_comments(detail, Some("🤖 ai:vetter")),
        "producerComments": trusted_comments(detail, Some("🤖 ai:producer")),
        "diffBytes": diff.len(),
        "diffIncluded": diff_text.len(),
        "diffTruncated": truncated,
        "diff": diff_text,
    })
}

/// Live `pr_context`: the PR detail, its diff, and every issue it Closes/Refs — three `gh` shapes,
/// one call, none of it re-derived in the model's context.
fn pr_context_fetch(slug: &str, num: u64, max_diff_bytes: usize) -> Result<Value, String> {
    let n = num.to_string();
    let detail = gh_json(&[
        "pr", "view", &n, "-R", slug, "--json",
        "number,title,body,url,headRefOid,isDraft,labels,reviewDecision,mergeable,statusCheckRollup,additions,deletions,files,closingIssuesReferences,comments",
    ])
    .ok_or_else(|| format!("error: `gh pr view {slug}#{num}` failed"))?;
    let diff = gh_text(&["pr", "diff", &n, "-R", slug])
        .ok_or_else(|| format!("error: `gh pr diff {slug}#{num}` failed"))?;
    let mut issues: Vec<Value> = Vec::new();
    for r in detail
        .get("closingIssuesReferences")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(inum) = r.get("number").and_then(|n| n.as_u64()) else {
            continue;
        };
        // A linked issue can live in another repo of the org; the reference carries its own repo.
        let islug = r
            .pointer("/repository/nameWithOwner")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| slug.to_string());
        let Some(mut iss) = gh_json(&[
            "issue",
            "view",
            &inum.to_string(),
            "-R",
            &islug,
            "--json",
            "number,title,body,labels,state",
        ]) else {
            continue;
        };
        if let Some(o) = iss.as_object_mut() {
            o.insert("repo".into(), Value::String(islug));
        }
        issues.push(iss);
    }
    fit_pr_context(slug, num, &detail, &diff, &issues, max_diff_bytes)
}

/// PURE (given its inputs): build the `pr_context` document so that it FITS
/// [`MCP_MAX_RESULT_BYTES`], shrinking the diff until it does.
///
/// This is what makes "no result can exceed the ceiling" a property of the tool rather than a hope
/// about its arguments (#81). Truncating the diff is not a silent loss: `max_diff_bytes` is a diff
/// cap by definition, and the document reports `diffBytes` (the whole diff's size), `diffIncluded`
/// (what actually made it in) and `diffTruncated`, so a caller can always see the difference between
/// what exists and what it was handed.
///
/// TERMINATION. Each round sets `cap -= overflow` where `overflow = len - MCP_MAX_RESULT_BYTES >= 1`,
/// so `cap` strictly decreases in a well-ordered set and the loop cannot run more than `cap` times.
/// It converges far faster than that bound: removing one raw byte of diff removes at least one byte
/// of serialized document (every raw byte contributes >= 1 serialized byte, more when escaped), so
/// one round overshoots rather than undershoots and two rounds is the practical worst case.
///
/// If the diff reaches zero and the document STILL does not fit, the metadata alone — body, file
/// list, linked issues, trusted comments — is over the ceiling. No argument can shrink that, so it
/// is an error that says so rather than a smaller diff nobody asked for.
fn fit_pr_context(
    slug: &str,
    num: u64,
    detail: &Value,
    diff: &str,
    issues: &[Value],
    max_diff_bytes: usize,
) -> Result<Value, String> {
    let mut cap = max_diff_bytes.min(diff.len());
    loop {
        let doc = pr_context_doc(slug, num, detail, diff, issues, cap);
        let len = doc.to_string().len();
        if len <= MCP_MAX_RESULT_BYTES {
            return Ok(doc);
        }
        if cap == 0 {
            return Err(format!(
                "error: `pr_context` for {slug}#{num} is {len} bytes with NO diff at all, over the \
                 {MCP_MAX_RESULT_BYTES}-byte budget one tool result must fit in. The overflow is \
                 metadata — body, file list, linked issues, trusted comments — so `max_diff_bytes` \
                 cannot shrink it and re-calling narrower will not help. Read this PR from its \
                 `pr_checkout` source and the issue links directly, or record NO verdict for it and \
                 name it in your run summary."
            ));
        }
        cap = cap.saturating_sub(len - MCP_MAX_RESULT_BYTES);
    }
}

/// The throwaway work-clone root for the vetter's audit lens (`WORK_DIR`, else the system temp dir).
fn vet_work_dir() -> String {
    std::env::var("WORK_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string()
    })
}

/// The name prefix every audit-lens checkout carries. It is what tells the unattended sweep that a
/// clone is a THROWAWAY the vetter created, not work someone is doing — see [`gc_decision`].
const VET_CLONE_PREFIX: &str = "vet-";

/// PURE: the per-PR throwaway clone path — the `vet-<repo>-<n>` convention `gc-clones` already
/// reclaims, so an MCP-driven checkout is garbage-collected exactly like a hand-rolled one.
fn checkout_dir(work_dir: &str, slug: &str, num: u64) -> String {
    let repo = slug.rsplit('/').next().unwrap_or(slug);
    format!(
        "{}/{VET_CLONE_PREFIX}{repo}-{num}",
        work_dir.trim_end_matches('/')
    )
}

/// PURE: the local branch an audit-lens checkout lands on. Derived from the PR NUMBER alone, so no
/// extra API round trip is needed to learn the head ref's name, and one code path serves same-repo
/// and fork PRs alike.
fn checkout_branch(num: u64) -> String {
    format!("pr-{num}")
}

/// PURE: the refspec that fetches ONE PR's head into a remote-tracking ref.
///
/// `refs/pull/<n>/head` is the ref to use, not `refs/heads/<headRefName>`:
///
/// - it exists on the BASE repo for every PR, fork or not, so forks need no second remote;
/// - it needs no `gh pr view` round trip to discover a branch name;
/// - the destination is under `refs/remotes/`, which is what makes the checked-out commit count as
///   PUSHED. `gh pr checkout` on a FORK PR lands the head on a plain LOCAL branch, so
///   `rev-list HEAD --not --remotes` reports the whole branch as unpushed and both `clone_release`
///   and the sweep then refuse to reclaim the clone — forever (#81).
fn pr_head_refspec(num: u64) -> String {
    format!("+refs/pull/{num}/head:refs/remotes/origin/pr/{num}")
}

/// PURE: the failure a caller of `pr_checkout` must act on. It is an ERROR (`isError: true`), and it
/// spends its words on the ONE thing the model does next, because the observed failure was not the
/// checkout — it was what the vetter did after it (#81).
///
/// Having no tree, the vetter globbed for one, found a `vet-*` leftover from an unrelated run, and
/// began enumerating THAT repo's sources as if they were the PR's. So the message states the
/// postcondition (the dir does not exist), names the specific wrong answer a filesystem search
/// returns, and gives the only two legal next moves.
fn checkout_failure_error(pr: &str, dir: &str, why: &str) -> String {
    format!(
        "error: `pr_checkout` could not produce a working tree for {pr}: {why}. There is NO \
         checkout — {dir} does not exist, and nothing was left behind at that path. Do NOT search \
         the filesystem for a substitute tree: a `{VET_CLONE_PREFIX}*` directory is some OTHER \
         PR's checkout, and a verdict read off it is a confident verdict about code this PR never \
         touched. Re-call `pr_checkout` ONCE; if it fails again you have no audit lens for this PR \
         — record NO verdict for it and name it in your run summary."
    )
}

/// Run `gh` for its exit status only, optionally inside `dir`, capturing BOTH streams (nothing leaks
/// to this process's stdout — the MCP JSON-RPC stream lives there).
fn gh_quiet(dir: Option<&std::path::Path>, args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("gh");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run gh {}: {e}", args.join(" ")))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = err.lines().rev().take(5).collect();
    Err(format!(
        "gh {} failed: {}",
        args.join(" "),
        tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
    ))
}

/// Land the PR's head commit in an EXISTING clone and return the sha. The half of the checkout that
/// touches the clone, split out so the shallow-clone behaviour #81 is about is testable against a
/// local repository instead of GitHub.
///
/// Two commands, no `gh`:
///
/// - the fetch carries an EXPLICIT destination refspec, which is what the shallow clone's
///   single-branch `remote.origin.fetch` cannot supply. `gh pr checkout` fetches without one, so the
///   head lands as a bare ref and `git checkout --track origin/<head>` refuses it — "cannot set up
///   tracking information", on every same-repo PR;
/// - `checkout -f -B` states the destination rather than deriving it from tracking configuration, so
///   there is nothing left to refuse, and a reused clone left half-modified is RESET to the PR head
///   rather than failing the checkout on a dirty tree.
///
/// Plain `git` is enough for auth: `gh repo clone` persists gh's credential helper into the clone's
/// own config, so the fetch is authenticated exactly as the clone was.
fn checkout_pr_head(path: &std::path::Path, num: u64) -> Result<String, String> {
    git_run(
        path,
        &["fetch", "--depth", "1", "origin", &pr_head_refspec(num)],
    )?;
    git_run(
        path,
        &[
            "checkout",
            "-f",
            "-B",
            &checkout_branch(num),
            &format!("refs/remotes/origin/pr/{num}"),
        ],
    )?;
    git_out(path, &["rev-parse", "HEAD"])
        .ok_or_else(|| "the checkout left no resolvable HEAD".to_string())
}

/// Check a PR out into its throwaway clone so the `audit` skill has SOURCE to read. LOCAL read only:
/// a shallow clone plus a shallow fetch of the PR head — never a push, a commit, or any GitHub
/// write. Reuses an existing clone (re-fetching the PR head into it) rather than re-cloning.
///
/// POSTCONDITION, and the whole point of #81: when this returns `Ok`, `dir` holds the PR's head
/// commit; when it returns `Err`, `dir` DOES NOT EXIST. There is no third state in which a directory
/// named after this PR sits on disk holding some other commit — which is exactly what the old code
/// left behind, because `gh repo clone --depth 1` succeeded (leaving the DEFAULT BRANCH checked out
/// at the canonical path) and only the subsequent `gh pr checkout` failed.
///
/// `gh pr checkout` is gone from this path. In a `--depth 1` clone the fetch refspec is
/// `+refs/heads/<default>:refs/remotes/origin/<default>`, so a same-repo PR's head arrives as a bare
/// fetched ref rather than a remote-tracking branch and `git checkout --track` refuses it with
/// "cannot set up tracking information" — for EVERY same-repo PR. See [`pr_head_refspec`] for why
/// `refs/pull/<n>/head` replaces it rather than a widened refspec: widening makes the follow-up fetch
/// deepen the clone to nearly full history (measured on raindex: 156 MiB of pack against 4 MiB),
/// which trades this bug for the disk-full one that silently killed both crons for ~17h (#56).
fn pr_checkout_exec(slug: &str, num: u64) -> Result<Value, String> {
    pr_checkout_at(&vet_work_dir(), slug, num)
}

/// [`pr_checkout_exec`] with the work root passed in rather than read from the environment, so the
/// postcondition above can be tested against a temp root without a process-global `set_var` racing
/// every other test in the binary.
fn pr_checkout_at(work_dir: &str, slug: &str, num: u64) -> Result<Value, String> {
    let pr = format!("{slug}#{num}");
    let dir = checkout_dir(work_dir, slug, num);
    let path = std::path::Path::new(&dir);
    let reused = path.join(".git").is_dir();
    if !reused && path.exists() {
        // The ONE failure that must not delete: this entry is not ours, so it is refused before
        // anything is touched — and the refusal says so rather than claiming the path is clear.
        return Err(format!(
            "error: `pr_checkout` could not produce a working tree for {pr}: {dir} exists but is \
             not a git clone — refusing to touch it. Move that path aside; nothing was changed."
        ));
    }
    let branch = checkout_branch(num);
    let build = || -> Result<String, String> {
        if !reused {
            gh_quiet(None, &["repo", "clone", slug, &dir, "--", "--depth", "1"])?;
        }
        checkout_pr_head(path, num)
    };
    let head = match build() {
        Ok(h) => h,
        Err(why) => {
            // Restore the postcondition. This is the file's only unguarded `remove_dir_all`, so
            // what makes it safe is stated rather than left to be re-derived: control only reaches
            // here past the check above, so `path` is either a directory that did NOT exist when
            // this call started (we made it) or one that holds `.git` (a clone we made on an earlier
            // call). Anything else was refused without being touched. `remove_dir_all` does not
            // follow a symlink, so a symlinked path cannot redirect the delete either.
            //
            // Best-effort: if it fails the path may still hold a wrong tree, so the message says so
            // instead of promising it is gone.
            if std::fs::remove_dir_all(path).is_err() && path.exists() {
                return Err(format!(
                    "error: `pr_checkout` could not produce a working tree for {pr}: {why}. \
                     {dir} could NOT be removed either and may hold a DIFFERENT commit — do not \
                     read it. Record no verdict for this PR and name it in your run summary."
                ));
            }
            return Err(checkout_failure_error(&pr, &dir, &why));
        }
    };
    Ok(serde_json::json!({
        "pr": pr,
        "dir": dir,
        "head": head,
        "branch": branch,
        "reused": reused,
        "note": "local read-only checkout for the audit lens. This `dir` is the ONLY tree that is this PR's source — never search for another. Release it with `clone_release` when done.",
    }))
}

// ─── MCP: the FSM as a tool surface ──────────────────────────────────────────

/// Server name. It becomes the middle segment of every exposed tool name — Claude Code presents an
/// MCP tool as `mcp__<server>__<tool>` and permission-matches it as `mcp__<server>__*` — so it is
/// SHORT on purpose: that string is repeated per tool in every request preamble.
const MCP_SERVER_NAME: &str = "fsm";
const MCP_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Protocol revisions this server speaks, newest first. The negotiated version is the client's
/// requested one when we know it, else [`MCP_PROTOCOL_DEFAULT`] — which every current client accepts.
const MCP_PROTOCOL_SUPPORTED: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
const MCP_PROTOCOL_DEFAULT: &str = "2025-06-18";

/// PURE: MCP version negotiation — echo the client's requested revision when supported, else offer
/// ours. Answering with an UNKNOWN revision is what makes a client hang up mid-handshake.
fn mcp_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|v| MCP_PROTOCOL_SUPPORTED.iter().find(|s| **s == v).copied())
        .unwrap_or(MCP_PROTOCOL_DEFAULT)
}

/// The vetter's verdicts — the ONLY values `record_verdict` accepts. Anything else (`approve`,
/// `merge`, `close-issue`, …) is not a transition of this machine and is refused.
const VETTER_VERDICTS: [&str; 5] = ["ready", "reject", "relink", "design", "close"];
/// Cost is a 0-1000 vibes score; a value outside it is a mis-scaled score, not a cost.
const COST_RANGE: std::ops::RangeInclusive<i64> = 0..=1000;
/// `basis` is a 3-8 word phrase naming the cost driver; a paragraph there is a note in the wrong slot.
const MAX_BASIS_WORDS: usize = 12;
/// The sweep's idle-clone age cap, and the bounds a tool caller may move it within. 0 would delete
/// every PR-less clone the instant it appeared, including one being built right now.
const GC_MAX_AGE_DEFAULT: u64 = 30;
const GC_MAX_AGE_RANGE: std::ops::RangeInclusive<u64> = 1..=365;

/// How many rows ONE state-load page carries, and the bounds a caller may move it within (#78).
///
/// The vetter judges ONE PR at a time and each `record_verdict` removes that PR from the next
/// call's page, so a page walks the whole queue without an offset argument — which is why the
/// default is small rather than "everything that fits". The ceiling is what keeps the bound
/// STRUCTURAL: at 25 rows a state-load cannot reach [`MCP_MAX_RESULT_BYTES`] even with GitHub's
/// longest legal titles, so the size of the queue stops being able to break the state-load.
const STATE_LOAD_PAGE_DEFAULT: usize = 10;
const STATE_LOAD_PAGE_RANGE: std::ops::RangeInclusive<u64> = 1..=25;

/// The byte budget ONE tool result must fit in — the contract this server holds itself to, checked
/// on every result before it is handed back (#78), and sized so that OUR error always arrives before
/// the harness's (#81).
///
/// That ordering is the whole mechanism. If the harness is the thing that speaks, the caller gets an
/// untyped message with `is_error` UNSET, and every rule downstream about "a tool error is an
/// instruction" stops applying at exactly the moment it is needed. The previous value was set by
/// halving a payload the harness had refused; that is how `pr_context`'s budget
/// (`max_diff_bytes + this`, up to 332,000) ended up six times above what the harness accepts, and
/// how the gap survived #79.
///
/// MEASURED against Claude Code 2.1.220, by calling `pr_context` through the real harness at
/// increasing `max_diff_bytes` and reading the `tool_result` the model actually received. There are
/// TWO independent gates, and BOTH arrive with `is_error` unset:
///
/// - a BYTE gate → `<persisted-output> Output too large (NN KB) … Preview (first 2KB)`. Delivered at
///   50,011 bytes, replaced at 50,176 — so the ceiling is in that 165-byte bracket. It is NOT
///   governed by `MAX_MCP_OUTPUT_TOKENS`: forcing that to 200,000 still replaced a 50,486-byte
///   result. This gate is the more dangerous one, because the 2 KB preview it substitutes LOOKS like
///   the head of a real answer.
/// - a TOKEN gate → `Error: result (N characters …) exceeds maximum allowed tokens`, governed by
///   `MAX_MCP_OUTPUT_TOKENS` (forcing it to 100 replaced a 4.5 KB result). This is the gate the live
///   traces hit, at 63,742 and 56,789 characters.
///
/// Isolating the token gate at `MAX_MCP_OUTPUT_TOKENS=10000` puts its boundary between 27,152 and
/// 30,163 bytes, so this JSON measures **2.7–3.0 chars/token**. Nothing on the box sets that
/// variable, and 56,789 characters was enough to trip the gate live, which puts the default cap near
/// 19–21k tokens — i.e. BOTH gates land around 50 kB for this content, and neither is far from the
/// other.
///
/// The value is set well under both rather than just under the tighter one, because the token gate
/// scales with the CONTENT: a diff of generated hex — which this org has, in every
/// `src/generated/*.pointers.sol` — tokenises far worse than prose, and a byte budget safe for one
/// is not automatically safe for the other. At 36,000 bytes even a payload tokenising at 1.5
/// chars/token stays inside a 19k-token cap. Re-measure with
/// `probe.sh <max_diff_bytes> [token-limit]` (see the PR) if the harness version moves.
const MCP_MAX_RESULT_BYTES: usize = 36_000;

/// The largest `pr_context` result observed DELIVERED through Claude Code 2.1.220. The next probe
/// up, 50,176 bytes, came back as `<persisted-output>` — so the harness's byte gate lives in that
/// 165-byte bracket. Recorded next to the budget it constrains, rather than in a comment that can
/// drift away from the number it is about.
const MEASURED_HARNESS_GATE_BYTES: usize = 50_011;

// The ordering that IS the mechanism, enforced at COMPILE time: our guard fires before the
// harness's, or this does not build. A budget raised past the gate silently reinstates #81 — the
// harness speaks instead, with `is_error` unset, and every downstream rule about tool errors stops
// applying. The 25% margin is not timidity: the token gate scales with CONTENT, and generated-hex
// diffs tokenise far worse than the prose-and-code JSON the gate was measured on.
const _: () = assert!(MCP_MAX_RESULT_BYTES < MEASURED_HARNESS_GATE_BYTES);
const _: () = assert!(MCP_MAX_RESULT_BYTES * 4 <= MEASURED_HARNESS_GATE_BYTES * 3);
// …and no argument may reach past the budget, which is what let `pr_context` sit six times above the
// harness ceiling however the guard was worded.
const _: () = assert!(MAX_MAX_DIFF_BYTES as usize <= MCP_MAX_RESULT_BYTES);
const _: () = assert!(DEFAULT_MAX_DIFF_BYTES <= MCP_MAX_RESULT_BYTES);

/// WHICH ROLE this server is serving. The two roles are different state machines that happen to
/// share a binary: the vetter judges PRs, the producer builds them. A profile is a SURFACE filter,
/// not a permission — `tools/list` returns only the profile's tools, so the producer never sees
/// `record_verdict` (the vetter's write) and the vetter never sees `clone_create`, and neither pays
/// preamble for the other's schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpProfile {
    Vetter,
    Producer,
}

impl McpProfile {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "vetter" => Ok(McpProfile::Vetter),
            "producer" => Ok(McpProfile::Producer),
            _ => Err(format!("unknown profile {s:?} — use vetter or producer")),
        }
    }
    /// The tool names this profile exposes, in listing order.
    fn tool_names(self) -> &'static [&'static str] {
        match self {
            // `clone_release` is on BOTH: `pr_checkout` creates a clone, so the vetter needs the
            // move that disposes of it — otherwise every vetted PR leaks a checkout until the sweep.
            McpProfile::Vetter => &[
                "unvetted",
                "pr_context",
                "pr_checkout",
                "record_verdict",
                "clone_release",
                // The vetter's SECOND subject: the producer also emits close-candidate flags on
                // issues, and a bad one asks a human to destroy work. Same three moves as a PR —
                // state-load, read one, record one verdict.
                "unvetted_close_candidates",
                "close_candidate_context",
                "record_close_candidate_verdict",
            ],
            McpProfile::Producer => &["clone_create", "clone_release", "clone_list", "clone_gc"],
        }
    }
}

/// The TOOL SURFACE. Descriptions are one line each on purpose: every schema here is re-sent in the
/// preamble of every API call, so the surface must replace more prose than it adds.
fn mcp_tools(profile: McpProfile) -> Value {
    let names = profile.tool_names();
    let all = mcp_all_tools();
    let all = all.as_array().expect("tool table is an array");
    Value::Array(
        names
            .iter()
            .map(|n| {
                all.iter()
                    .find(|t| t["name"] == Value::String((*n).to_string()))
                    .unwrap_or_else(|| panic!("profile names an undefined tool {n:?}"))
                    .clone()
            })
            .collect(),
    )
}

fn mcp_all_tools() -> Value {
    serde_json::json!([
        {
            "name": "unvetted",
            "description": "State-load: ONE PAGE of the open PRs to vet, vet-first order. Per PR: headRefOid, labels, reviewDecision, humanSacred, vettedAtHead, ci, mergeable. `counts` is whole-queue; `more` is how many vet-able PRs this page left behind — re-call after recording verdicts for the next page. `openThreads` lists the PRs withheld because a review thread is unresolved. Human-decided, draft and vetted-at-head PRs are already excluded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_skipped": {"type": "boolean", "description": "Also list the excluded PRs and why (digest rows: pr, action, unresolvedThreads)."},
                    "limit": {"type": "integer", "description": "Rows per list, 1-25 (default 10)."}
                }
            }
        },
        {
            "name": "pr_context",
            "description": "Everything needed to judge one PR: title, body, files, additions/deletions, headRefOid, ci, mergeable, the full diff, every linked issue's title/body/labels, and the trusted ai:vetter/ai:producer comments.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pr": {"type": "string", "description": "owner/repo#number"},
                    "max_diff_bytes": {"type": "integer", "description": "Diff cap, default 300000."}
                },
                "required": ["pr"]
            }
        },
        {
            "name": "pr_checkout",
            "description": "Check the PR head out into a throwaway local clone so the audit skill can read source. Local read only — no GitHub write. Returns its dir.",
            "inputSchema": {
                "type": "object",
                "properties": {"pr": {"type": "string", "description": "owner/repo#number"}},
                "required": ["pr"]
            }
        },
        {
            "name": "record_verdict",
            "description": "The vetter's ONLY write: apply ai:<verdict> (removing any other ai:*) + a sha-bound ai:vetter comment carrying the cost. Refuses if a human has decided the PR.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pr": {"type": "string", "description": "owner/repo#number"},
                    "verdict": {"type": "string", "enum": ["ready", "reject", "relink", "design", "close"]},
                    "note": {"type": "string", "description": "One line naming the issue number(s) and the specific reason."},
                    "cost": {"type": "integer", "description": "Human verification cost, 0-1000."},
                    "basis": {"type": "string", "description": "3-8 words naming the cost driver."}
                },
                "required": ["pr", "verdict", "note", "cost", "basis"]
            }
        },
        {
            "name": "unvetted_close_candidates",
            "description": "State-load: ONE PAGE of the producer close-candidate flags on open issues to vet. Per issue: flagAt, flagReason (the producer's stated evidence), labels, humanSacred, vettedAtFlag. `counts` is whole-queue; `more` is how many this page left behind — re-call after recording verdicts. Human-ruled and already-vetted-at-flag issues are excluded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_skipped": {"type": "boolean", "description": "Also list the excluded issues and why."},
                    "limit": {"type": "integer", "description": "Rows per list, 1-25 (default 10)."}
                }
            }
        },
        {
            "name": "close_candidate_context",
            "description": "Everything needed to judge one close-candidate flag: the issue's title, body, labels, createdAt, state, and the trusted ai:producer flag(s) + any prior ai:vetter verdicts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "issue": {"type": "string", "description": "owner/repo#number"}
                },
                "required": ["issue"]
            }
        },
        {
            "name": "record_close_candidate_verdict",
            "description": "The vetter's write on a flag: uphold (leave it queued for the human) or reject (drop ai:close-candidate, returning the issue to the producer). Posts a flag-pinned ai:vetter comment. Refuses if a human has ruled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "issue": {"type": "string", "description": "owner/repo#number"},
                    "verdict": {"type": "string", "enum": ["uphold", "reject"]},
                    "note": {"type": "string", "description": "One line: what the evidence proves, or which check it fails (recency / reachability / scope)."}
                },
                "required": ["issue", "verdict", "note"]
            }
        },
        {
            "name": "clone_create",
            "description": "Make (or re-sync to current base) the per-issue work clone. Returns its dir. Refuses to re-sync over unpushed commits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/repo"},
                    "name": {"type": "string", "description": "Clone dir name, e.g. raindex-2444. One path segment; the root is fixed."},
                    "branch": {"type": "string", "description": "Branch to create/reset, e.g. 2026-07-22-issue-2444."},
                    "base": {"type": "string", "description": "Base branch; defaults to the repo's default branch."}
                },
                "required": ["repo", "name", "branch"]
            }
        },
        {
            "name": "clone_release",
            "description": "Dispose of a work clone the moment its work is on GitHub. This replaces `rm -rf`. Refuses unpushed commits outright; refuses uncommitted changes unless discard_uncommitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "clone": {"type": "string", "description": "Clone dir name (or its full path under the work root)."},
                    "discard_uncommitted": {"type": "boolean", "description": "Release even with uncommitted changes — only once you have confirmed they are build/tooling output."}
                },
                "required": ["clone"]
            }
        },
        {
            "name": "clone_list",
            "description": "Every work clone on this box: name, branch, unpushed/uncommitted counts, age, and whether it is releasable now.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "clone_gc",
            "description": "End-of-run sweep of every work root: deletes only clean, fully-pushed clones whose PR is merged/closed, or PR-less clones idle past the age cap. Reports bytes reclaimed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dry_run": {"type": "boolean", "description": "Report the decisions without deleting."},
                    "max_age_days": {"type": "integer", "description": "Idle cap for PR-less clones, 1-365 (default 30)."}
                }
            }
        }
    ])
}

/// A VALIDATED transition. Constructing one is the only way to reach an effect, so an invalid
/// transition cannot be represented — the point of #52.
#[derive(Debug, PartialEq, Eq, Clone)]
enum McpCall {
    Unvetted {
        include_skipped: bool,
        /// Page size. Always `Some` from the MCP guard — the token-budgeted caller never gets an
        /// unbounded state-load, which is the whole of #78.
        limit: usize,
    },
    PrContext {
        slug: String,
        num: u64,
        max_diff_bytes: usize,
    },
    PrCheckout {
        slug: String,
        num: u64,
    },
    RecordVerdict {
        slug: String,
        num: u64,
        verdict: String,
        note: String,
        cost: i64,
        basis: String,
    },
    UnvettedCloseCandidates {
        include_skipped: bool,
        limit: usize,
    },
    CloseCandidateContext {
        slug: String,
        num: u64,
    },
    RecordCloseCandidateVerdict {
        slug: String,
        num: u64,
        verdict: String,
        note: String,
    },
    /// `root`/`name` are the OUTPUT of the path guard, not the model's argument: by the time this
    /// value exists, the name is known to be a single non-hidden component under a configured root.
    CloneCreate {
        root: String,
        name: String,
        slug: String,
        branch: String,
        base: Option<String>,
    },
    CloneRelease {
        root: String,
        name: String,
        discard_uncommitted: bool,
    },
    CloneList,
    CloneGc {
        max_age_days: u64,
        dry_run: bool,
    },
}

/// PURE: `owner/repo#number` → (slug, number). One string keeps the schemas small AND makes the
/// "always use the PR's ACTUAL owner/repo" rule structural — there is no org to guess wrong.
fn parse_pr_ref(s: &str) -> Result<(String, u64), String> {
    let bad =
        || format!("bad pr ref {s:?} — want owner/repo#number, e.g. rainlanguage/rain.flare#170");
    let (slug, num) = s.trim().split_once('#').ok_or_else(bad)?;
    let num: u64 = num.trim().parse().map_err(|_| bad())?;
    let mut parts = slug.trim().split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    if owner.is_empty() || repo.is_empty() || num == 0 {
        return Err(bad());
    }
    Ok((format!("{owner}/{repo}"), num))
}

/// PURE: a state-load's page size. Absent means [`STATE_LOAD_PAGE_DEFAULT`]; out of range is
/// REFUSED rather than clamped, for the same reason `max_diff_bytes` is — a silently-clamped
/// argument leaves the caller believing it asked for something it did not get, which is the class
/// of quiet disagreement #78 is about.
fn state_load_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(STATE_LOAD_PAGE_DEFAULT),
        Some(v) => match v.as_u64() {
            Some(n) if STATE_LOAD_PAGE_RANGE.contains(&n) => Ok(n as usize),
            _ => Err(format!(
                "limit must be an integer in {}..={}",
                STATE_LOAD_PAGE_RANGE.start(),
                STATE_LOAD_PAGE_RANGE.end()
            )),
        },
    }
}

/// PURE: the byte budget THIS call's result must fit in — [`MCP_MAX_RESULT_BYTES`], for EVERY tool.
///
/// `pr_context` used to be an exception: its budget was `max_diff_bytes + MCP_MAX_RESULT_BYTES`, on
/// the reasoning that a big result there is what the caller asked for. Two things were wrong with
/// that (#81). It let the budget reach 332,000 bytes — six times what the harness accepts — so the
/// guard could not fire and the harness's untyped message arrived instead. And because the diff is
/// TRUNCATED to `max_diff_bytes`, budget and content moved in lockstep: lowering the argument
/// lowered the allowance by exactly as much as it lowered the payload, so "re-call NARROWER" was a
/// loop with no exit.
///
/// A budget that does not move with the argument is what makes narrowing converge, and it is why
/// this function no longer looks at the call at all. It takes one anyway, so the ONE budget stays a
/// property of the request rather than a constant callers reach past.
fn call_result_budget(_call: &McpCall) -> usize {
    MCP_MAX_RESULT_BYTES
}

/// PURE: which argument actually makes THIS call's result fit.
///
/// Every answer here is now truthful because [`call_result_budget`] does not move with the argument:
/// lowering the named argument lowers the payload against a FIXED allowance, so narrowing strictly
/// converges. While `pr_context`'s budget still scaled with `max_diff_bytes`, naming that argument
/// was advice that could not work — budget and content fell together and the caller could loop for
/// ever — which is why the fix is the budget, not the wording.
fn narrowing_argument(call: &McpCall) -> Option<&'static str> {
    match call {
        McpCall::PrContext { .. } => Some("max_diff_bytes"),
        _ => Some("limit"),
    }
}

/// PURE: the over-budget refusal. It is an ERROR, not a truncation and not a spill, and it names the
/// argument that makes the call smaller — the caller's next move must be a narrower call, not an
/// improvised one. (#78: the vetter met an over-budget state-load, invented a fallback that dropped
/// the open-threads accounting, and nothing in the run said so.) When NO argument makes it smaller
/// it says that instead, and names the only honest move left.
fn oversize_result_error(name: &str, len: usize, budget: usize, narrow: Option<&str>) -> String {
    let head = format!(
        "error: tool `{name}` produced {len} bytes, over the {budget}-byte budget one tool result \
         must fit in. Nothing was truncated or spilled — a partial state-load cannot say what it is \
         missing. "
    );
    match narrow {
        Some(arg) => format!("{head}Re-call NARROWER: lower `{arg}`."),
        None => format!(
            "{head}NO argument makes this call smaller — `max_diff_bytes` caps the diff and raises \
             this budget by the same amount, so this result is over budget on its METADATA alone. \
             Do not retry it with a different `max_diff_bytes` expecting a different answer, and do \
             not improvise a substitute read: record NO verdict for this PR and name it in your run \
             summary."
        ),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!("missing required string argument {key:?}")),
    }
}

/// PURE: the TRANSITION GUARD. Maps (tool name, arguments) to a validated [`McpCall`], or to the
/// error the model sees. Every rule the vetter prompt used to state in prose — the verdict
/// vocabulary, a mandatory cost in range, a note that says something, a phrase-length basis, a
/// well-formed PR reference — is enforced HERE, once, tested.
fn validate_call(
    profile: McpProfile,
    roots: &[String],
    name: &str,
    args: &Value,
) -> Result<McpCall, String> {
    // A tool the profile does not expose does not exist for this role — checked FIRST, so the
    // producer cannot reach `record_verdict` and the vetter cannot reach `clone_create` by name.
    if !profile.tool_names().contains(&name) {
        return Err(format!(
            "no such tool {name:?} — this server exposes exactly: {}",
            profile.tool_names().join(", ")
        ));
    }
    match name {
        "unvetted" => Ok(McpCall::Unvetted {
            include_skipped: args
                .get("include_skipped")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            limit: state_load_limit(args)?,
        }),
        "pr_context" => {
            let (slug, num) = parse_pr_ref(req_str(args, "pr")?)?;
            let max_diff_bytes = match args.get("max_diff_bytes") {
                None | Some(Value::Null) => DEFAULT_MAX_DIFF_BYTES,
                Some(v) => match v.as_u64() {
                    Some(n) if n > 0 && n <= MAX_MAX_DIFF_BYTES => n as usize,
                    _ => {
                        return Err(format!(
                            "max_diff_bytes must be an integer in 1..={MAX_MAX_DIFF_BYTES}"
                        ))
                    }
                },
            };
            Ok(McpCall::PrContext {
                slug,
                num,
                max_diff_bytes,
            })
        }
        "pr_checkout" => {
            let (slug, num) = parse_pr_ref(req_str(args, "pr")?)?;
            Ok(McpCall::PrCheckout { slug, num })
        }
        "record_verdict" => {
            let (slug, num) = parse_pr_ref(req_str(args, "pr")?)?;
            let verdict = req_str(args, "verdict")?.trim().to_string();
            if !VETTER_VERDICTS.contains(&verdict.as_str()) {
                return Err(format!(
                    "{verdict:?} is not a verdict of this machine — use one of: {}",
                    VETTER_VERDICTS.join(", ")
                ));
            }
            let note = req_str(args, "note")?.trim().to_string();
            let cost = args.get("cost").and_then(|v| v.as_i64()).ok_or_else(|| {
                "cost is required: an integer 0-1000 (human verification cost)".to_string()
            })?;
            if !COST_RANGE.contains(&cost) {
                return Err(format!("cost {cost} is out of range — an integer 0-1000"));
            }
            let basis = req_str(args, "basis")?.trim().to_string();
            if basis.split_whitespace().count() > MAX_BASIS_WORDS {
                return Err(format!(
                    "basis must be at most {MAX_BASIS_WORDS} words naming the cost driver (put the reasoning in note)"
                ));
            }
            Ok(McpCall::RecordVerdict {
                slug,
                num,
                verdict,
                note,
                cost,
                basis,
            })
        }
        "unvetted_close_candidates" => Ok(McpCall::UnvettedCloseCandidates {
            include_skipped: args
                .get("include_skipped")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            limit: state_load_limit(args)?,
        }),
        "close_candidate_context" => {
            let (slug, num) = parse_pr_ref(req_str(args, "issue")?)?;
            Ok(McpCall::CloseCandidateContext { slug, num })
        }
        "record_close_candidate_verdict" => {
            let (slug, num) = parse_pr_ref(req_str(args, "issue")?)?;
            let verdict = req_str(args, "verdict")?.trim().to_string();
            if !CC_VERDICTS.contains(&verdict.as_str()) {
                return Err(format!(
                    "{verdict:?} is not a close-candidate verdict — use one of: {}",
                    CC_VERDICTS.join(", ")
                ));
            }
            let note = req_str(args, "note")?.trim().to_string();
            if note.is_empty() {
                return Err(
                    "note is required: one line naming what the evidence proves or fails to prove"
                        .to_string(),
                );
            }
            Ok(McpCall::RecordCloseCandidateVerdict {
                slug,
                num,
                verdict,
                note,
            })
        }
        // --- work-clone lifecycle. The path guard runs HERE, before any effect exists, which is why
        // a refused clone argument can be proven to have touched nothing.
        "clone_create" => {
            let slug = req_str(args, "repo")?.trim().to_string();
            let mut parts = slug.split('/');
            let (Some(o), Some(r), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(format!("bad repo {slug:?} — want owner/repo"));
            };
            if o.is_empty() || r.is_empty() {
                return Err(format!("bad repo {slug:?} — want owner/repo"));
            }
            // clone_create always builds in the FIRST root (WORK_DIR); the extra roots exist so
            // already-stranded clones can be listed/released, not so new ones can be placed there.
            let root = roots.first().ok_or_else(|| {
                "no work-clone root is configured (WORK_DIR is unset)".to_string()
            })?;
            let name = clone_name_in_root(root, req_str(args, "name")?)?;
            let branch = req_str(args, "branch")?.trim().to_string();
            if branch.contains(char::is_whitespace) || branch.starts_with('-') {
                return Err(format!("bad branch {branch:?}"));
            }
            let base = match args.get("base") {
                None | Some(Value::Null) => None,
                Some(_) => Some(req_str(args, "base")?.trim().to_string()),
            };
            Ok(McpCall::CloneCreate {
                root: root.trim_end_matches('/').to_string(),
                name,
                slug,
                branch,
                base,
            })
        }
        "clone_release" => {
            let (root, name) = clone_in_roots(roots, req_str(args, "clone")?)?;
            Ok(McpCall::CloneRelease {
                root,
                name,
                discard_uncommitted: args
                    .get("discard_uncommitted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        }
        "clone_list" => Ok(McpCall::CloneList),
        "clone_gc" => {
            let max_age_days = match args.get("max_age_days") {
                None | Some(Value::Null) => GC_MAX_AGE_DEFAULT,
                Some(v) => match v.as_u64() {
                    Some(n) if GC_MAX_AGE_RANGE.contains(&n) => n,
                    _ => {
                        return Err(format!(
                            "max_age_days must be an integer in {}..={}",
                            GC_MAX_AGE_RANGE.start(),
                            GC_MAX_AGE_RANGE.end()
                        ))
                    }
                },
            };
            Ok(McpCall::CloneGc {
                max_age_days,
                dry_run: args
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        }
        // Unreachable while `tool_names()` and this match agree (pinned by a test). An Err rather
        // than a panic: a listed-but-unhandled tool must not take the whole server down.
        _ => Err(format!("tool {name:?} is listed but not implemented")),
    }
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A `tools/call` result. A REFUSED transition is a successful JSON-RPC response carrying
/// `isError: true` — the model reads the reason and corrects, exactly as it would a tool's own error.
fn tool_result(text: String, is_error: bool) -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    })
}

/// Handle ONE JSON-RPC message. Pure apart from `exec`, which performs a validated call — so the
/// whole protocol surface (handshake, listing, dispatch, refusals) is unit-testable with a fake exec.
/// `None` = a notification, which is never answered.
fn mcp_handle(
    profile: McpProfile,
    roots: &[String],
    req: &Value,
    exec: &mut dyn FnMut(McpCall) -> Result<String, String>,
) -> Option<Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = match req.get("id") {
        None | Some(Value::Null) => return None,
        Some(v) => v.clone(),
    };
    match method {
        "initialize" => Some(jsonrpc_result(
            id,
            serde_json::json!({
                "protocolVersion": mcp_protocol_version(
                    req.pointer("/params/protocolVersion").and_then(|v| v.as_str())
                ),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": MCP_SERVER_NAME, "version": MCP_SERVER_VERSION},
            }),
        )),
        "tools/list" => Some(jsonrpc_result(
            id,
            serde_json::json!({"tools": mcp_tools(profile)}),
        )),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let out = match validate_call(profile, roots, name, &args) {
                Err(e) => tool_result(e, true),
                Ok(call) => {
                    // The budget AND the narrowing advice are read off the VALIDATED call, before the
                    // effect runs, because `exec` consumes it — and because both are properties of
                    // what was asked for.
                    let budget = call_result_budget(&call);
                    let narrow = narrowing_argument(&call);
                    match exec(call) {
                        // A result over budget is THIS server's error to raise. Handing it back and
                        // letting the harness reject it is what left the vetter improvising (#78).
                        Ok(text) if text.len() > budget => tool_result(
                            oversize_result_error(name, text.len(), budget, narrow),
                            true,
                        ),
                        Ok(text) => tool_result(text, false),
                        Err(e) => tool_result(e, true),
                    }
                }
            };
            Some(jsonrpc_result(id, out))
        }
        "ping" => Some(jsonrpc_result(id, serde_json::json!({}))),
        // resources/prompts are not offered (no such capability was advertised).
        _ => Some(jsonrpc_error(
            id,
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

/// Perform a validated transition. The ONLY effectful half of the server; every guard already ran.
fn mcp_exec(call: McpCall) -> Result<String, String> {
    let roots = clone_roots();
    match call {
        McpCall::Unvetted {
            include_skipped,
            limit,
        } => unvetted_fetch(include_skipped, Some(limit)).map(|d| d.to_string()),
        McpCall::PrContext {
            slug,
            num,
            max_diff_bytes,
        } => pr_context_fetch(&slug, num, max_diff_bytes).map(|d| d.to_string()),
        McpCall::PrCheckout { slug, num } => pr_checkout_exec(&slug, num).map(|d| d.to_string()),
        McpCall::RecordVerdict {
            slug,
            num,
            verdict,
            note,
            cost,
            basis,
        } => record_verdict_apply(
            &slug,
            &num.to_string(),
            &verdict,
            &note,
            Some(cost),
            &basis,
            false,
        )
        .map_err(|(code, msg)| format!("{msg} [exit {code}]")),
        McpCall::UnvettedCloseCandidates {
            include_skipped,
            limit,
        } => unvetted_close_candidates_fetch(include_skipped, Some(limit)).map(|d| d.to_string()),
        McpCall::CloseCandidateContext { slug, num } => {
            close_candidate_context_fetch(&slug, num).map(|d| d.to_string())
        }
        McpCall::RecordCloseCandidateVerdict {
            slug,
            num,
            verdict,
            note,
        } => record_cc_verdict_apply(&slug, &num.to_string(), &verdict, &note, false)
            .map_err(|(code, msg)| format!("{msg} [exit {code}]")),
        McpCall::CloneCreate {
            root,
            name,
            slug,
            branch,
            base,
        } => {
            clone_create_exec(&root, &name, &slug, &branch, base.as_deref()).map(|d| d.to_string())
        }
        McpCall::CloneRelease {
            root,
            name,
            discard_uncommitted,
        } => clone_release_exec(&root, &name, discard_uncommitted).map(|d| d.to_string()),
        McpCall::CloneList => clone_list_exec(&roots).map(|d| d.to_string()),
        McpCall::CloneGc {
            max_age_days,
            dry_run,
        } => clone_gc_exec(&roots, max_age_days, dry_run).map(|d| d.to_string()),
    }
}

/// `pr-review-report mcp` — speak MCP over stdio (newline-delimited JSON-RPC 2.0 on stdin/stdout).
/// STDOUT IS THE PROTOCOL: nothing else may print there, which is why the verdict write goes through
/// [`record_verdict_apply`] rather than the printing CLI mode.
fn mcp_serve(profile: McpProfile) -> i32 {
    use std::io::{BufRead, Write};
    let roots = clone_roots();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(line) {
            Ok(req) => mcp_handle(profile, &roots, &req, &mut mcp_exec),
            Err(e) => Some(jsonrpc_error(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(r) = resp {
            if writeln!(out, "{r}").is_err() || out.flush().is_err() {
                return 1;
            }
        }
    }
    0
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
    /// Garbage-collect the per-PR/issue work clones directly under each <work-dir>.
    GcClones {
        /// One or more clone roots. More than one because clones do not all land in WORK_DIR — the
        /// vetter's `vet-*` clones accumulated in the INSTALL dir, which a single-root sweep missed.
        #[arg(required = true, num_args = 1..)]
        work_dirs: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 30)]
        max_age_days: u64,
    },
    /// Unified reclaim: the work clones (gc-clones), always; the nix store (nix-collect-garbage -d)
    /// only when the disk is under pressure (usage >= --nix-threshold), so the build cache stays warm.
    Gc {
        /// One or more clone roots. Required unless --no-clones.
        work_dirs: Vec<String>,
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
    RunMetrics {
        trace: String,
        /// Run id (the runner's UTC timestamp). Enriches the record with `runId`.
        #[arg(long)]
        run_id: Option<String>,
        /// producer | vetter.
        #[arg(long)]
        role: Option<String>,
        /// The model that actually ran (after any fallback).
        #[arg(long)]
        model: Option<String>,
        /// claude's exit code. Also selects `outcome`, classified from the trace.
        #[arg(long)]
        exit_code: Option<i32>,
        /// Binaries `preflight` could not resolve, comma-separated. Present means the run was
        /// stopped before the model started, so there is no trace to classify — the outcome is
        /// `tooling-failure` on this fact alone.
        #[arg(long, value_delimiter = ',')]
        preflight_missing: Vec<String>,
    },
    /// Resolve every external binary the HARNESS needs at read time. Exit 12 if any is missing.
    ///
    /// Run before the model, by both runners. A dependency that is absent must stop the run, not
    /// degrade it: the failure that motivated this was a vetter that could not render an audit PDF,
    /// vetted the PR on what was left, and reported success (#85).
    Preflight,
    /// CI gate: every `HARNESS_TOOLS` entry resolves from EACH model runner's own baked PATH.
    /// Exit 12 if a closure is missing one, 2 if a runner could not be built or read.
    ClosurePreflight {
        /// Flake to build the model runners from.
        #[arg(long, default_value = ".")]
        flake: String,
    },
    /// CI gate: each model runner's closure RENDERS a generated PDF with the harness's own argv —
    /// presence is not capability, and a broken renderer reaches the model as the same `isError`
    /// an absent one does.
    ClosureRender {
        /// Flake to build the model runners from.
        #[arg(long, default_value = ".")]
        flake: String,
    },
    /// CI gate: the model runners' capability surfaces differ only where `DECLARED_ASYMMETRY`
    /// says, in both directions (an undeclared difference AND a declaration that is now false).
    ClosureSurface {
        /// Flake to build the model runners from.
        #[arg(long, default_value = ".")]
        flake: String,
    },
    /// Pass a LIVE stream-json trace through on stdout, appending `bootMs` then `ttlMs` records to
    /// the metrics file the instant each becomes known — so a killed or timed-out run, the one
    /// `run-metrics` can never reach, still contributes its startup timings.
    RunTimings {
        /// The metrics file to append partial records to — the same runs.jsonl the end-of-run
        /// `run-metrics` record lands in.
        #[arg(long)]
        out: String,
        /// The trace path the runner is teeing this same stream to, recorded as `trace` so a
        /// partial record identifies the same run its final record will.
        #[arg(long)]
        trace: String,
        /// Run id (the runner's UTC timestamp). Enriches the record with `runId`.
        #[arg(long)]
        run_id: Option<String>,
        /// producer | vetter.
        #[arg(long)]
        role: Option<String>,
        /// The model that actually ran (after any fallback).
        #[arg(long)]
        model: Option<String>,
    },
    /// Print the typed outcome of a run trace: `ok`, `session-limit`, or `error`.
    /// The runners' model-fallback loop branches on this instead of grepping the trace.
    TraceOutcome {
        trace: String,
        /// claude's exit code, so a run that died without a result event classifies as `error`.
        #[arg(long, default_value_t = 0)]
        exit_code: i32,
    },
    /// Emit one `{ts, counts}` rollup line from a human-queue.json snapshot (stdin if no path).
    /// Prints nothing when the snapshot predates the `counts` key.
    QueueHistoryLine {
        /// Snapshot path; omit to read stdin.
        snapshot: Option<String>,
        /// ISO-8601 timestamp for the line.
        #[arg(long)]
        ts: String,
    },
    /// Read a stream-json trace on stdin, write the human-readable run log on stdout.
    DistillTrace,
    /// Weekly-budget pace gate. Prints one line; exits 0 to RUN this tick, 10 to PAUSE it.
    /// Config is env-only (USAGE_CEILING_PCT, USAGE_SLACK_PCT, USAGE_USED_PCT, USAGE_RESET_AT,
    /// CLAUDE_CREDENTIALS, USAGE_URL), exported by the runners from cron.env.
    UsageGate,
    /// The producer's whole in-flight worklist in ONE call: own open PRs with CI/failing-checks/
    /// mergeState/threads/closes/markers and a computed next_action. Replaces the hand-rolled startup.
    Worklist {
        #[arg(long)]
        json: bool,
        /// Bypass the read-through cache entirely (always fetch fresh).
        #[arg(long)]
        no_cache: bool,
    },
    /// Open issues NOT already covered by an open PR — coverage from GitHub's native
    /// `closingIssuesReferences` (the same references the merge resolves), not body regexing.
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
    /// The VETTER's state-load in ONE call: the open PRs to vet this run (vet-first order), each with
    /// headRefOid/labels/reviewDecision/humanSacred/vettedAtHead/ci/mergeable.
    Unvetted {
        #[arg(long)]
        json: bool,
        /// Also list the skipped PRs (draft / human-decided / vetted-at-head) and why.
        #[arg(long)]
        include_skipped: bool,
        /// Rows per list. Omitted = unbounded: a terminal has no token budget, so the CLI is the
        /// one caller that may ask for the whole queue. The MCP surface always pages (#78).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Speak MCP over stdio, exposing a role's FSM transitions as tools — an agent restricted to
    /// this server cannot perform a non-FSM operation. Wiring: `review-mcp.json`, `campaign-mcp.json`.
    Mcp {
        /// Which role's surface to serve: `vetter` (default) or `producer`.
        #[arg(long, default_value = "vetter")]
        profile: String,
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
    /// The PR carries a human decision label (`human:reject` / `human:design` /
    /// `human:close-candidate`). A human decision is SACRED and blocks routine producer action, so
    /// such a PR is always parked — even when it also carries a stale `ai:*` label (a `human:reject`
    /// PR keeps its old `ai:ready` until `reworked-reject` clears it).
    has_human_override: bool,
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
    // A human decision (`human:reject`/`human:design`/`human:close-candidate`) is SACRED and blocks
    // routine producer action — park it regardless of any stale `ai:*` label it also carries (a
    // `human:reject` PR keeps its old `ai:ready` until `reworked-reject` clears it; a rework note is
    // handled by the reject-work-order path, not this routine classifier). This MUST come first so a
    // human-overridden PR is never re-derived from CI/mergeState.
    if s.has_human_override {
        return NextAction::ParkedSkip;
    }
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

/// PURE: the covered set from GraphQL PR-search nodes — one (repo, issue#) per native
/// `closingIssuesReferences` entry, keyed by the ISSUE's repository (a cross-repo reference
/// covers the referenced repo, not the PR's). PR title/body text is deliberately NOT parsed:
/// GitHub's resolved references are the coverage signal, so the URL form
/// (`Closes https://github.com/o/r/issues/5`) and `o/r#5` cover exactly what GitHub will
/// auto-close at merge, and a title-only keyword (which GitHub never links) counts nothing.
fn covered_from_search_prs(nodes: &[Value]) -> std::collections::HashSet<(String, u64)> {
    let mut covered = std::collections::HashSet::new();
    for pr in nodes {
        let Some(refs) = pr
            .pointer("/closingIssuesReferences/nodes")
            .and_then(|n| n.as_array())
        else {
            continue;
        };
        for r in refs {
            let (Some(repo), Some(num)) = (
                r.pointer("/repository/nameWithOwner")
                    .and_then(|s| s.as_str()),
                r.get("number").and_then(|n| n.as_u64()),
            ) else {
                continue;
            };
            covered.insert((repo.to_string(), num));
        }
    }
    covered
}

/// The GraphQL `search` connection serves at most 1000 results, so the page size asked for in
/// [`CLOSING_REFS_QUERY`] and the page cap [`SEARCH_MAX_PAGES`] must multiply out to exactly that.
/// A smaller product silently under-reports coverage; a larger one just walks into GitHub's own cap.
const SEARCH_PAGE_SIZE: usize = 100;
const SEARCH_MAX_PAGES: usize = 10;
const CLOSING_REFS_QUERY: &str = "query($q:String!,$c:String){search(query:$q,type:ISSUE,first:100,after:$c){pageInfo{hasNextPage endCursor}nodes{... on PullRequest{closingIssuesReferences(first:50){nodes{number repository{nameWithOwner}}}}}}}";

/// PURE given `fetch`: walk a cursor-paged GraphQL `search` to the end, concatenating every page's
/// `nodes`. `fetch(cursor)` performs one page (`None` on the first page, then the previous page's
/// `endCursor`).
///
/// Returns `(nodes, truncated)`. `truncated` is true when GitHub still reported `hasNextPage` and
/// the walk stopped anyway — either [`SEARCH_MAX_PAGES`] ran out, or the page reported no
/// `endCursor` to advance on. Both are the SAME hazard and the caller must treat them the same way:
/// unseen pages mean unseen coverage, and unseen coverage reads as *uncovered*, which makes the
/// producer open a duplicate PR. A failing page returns `None` for the whole walk, never a short
/// vec — a partial read is indistinguishable from a complete one once it is just a `Vec`.
///
/// The page cap is the constant, not an argument: it is a property of GitHub's `search` connection,
/// and a call site free to pass its own number is a call site free to cap coverage at one page.
fn paged_search_nodes<F>(mut fetch: F) -> Option<(Vec<Value>, bool)>
where
    F: FnMut(Option<&str>) -> Option<Value>,
{
    let mut nodes: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..SEARCH_MAX_PAGES {
        let v = fetch(cursor.as_deref())?;
        let search = v.pointer("/data/search")?;
        if let Some(arr) = search.get("nodes").and_then(|n| n.as_array()) {
            nodes.extend(arr.iter().cloned());
        }
        if !search
            .pointer("/pageInfo/hasNextPage")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            return Some((nodes, false));
        }
        match search
            .pointer("/pageInfo/endCursor")
            .and_then(|s| s.as_str())
        {
            Some(c) => cursor = Some(c.to_string()),
            // More pages exist and there is no cursor to reach them with.
            None => return Some((nodes, true)),
        }
    }
    Some((nodes, true))
}

/// All open PRs in the org scope with their native `closingIssuesReferences`, one paged GraphQL
/// search. `None` if any page fails — the caller must abort rather than treat unseen coverage as
/// uncovered (a false uncovered row makes the producer open a duplicate PR).
fn search_open_prs_closing_refs() -> Option<Vec<Value>> {
    let q = format!("q={}", org_search_scope());
    let query = format!("query={CLOSING_REFS_QUERY}");
    let (nodes, truncated) = paged_search_nodes(|cursor| {
        let mut args: Vec<&str> = vec!["api", "graphql", "-f", &query, "-f", &q];
        let cf;
        if let Some(c) = cursor {
            cf = format!("c={c}");
            args.push("-f");
            args.push(&cf);
        }
        gh_json(&args)
    })?;
    if truncated {
        eprintln!(
            "warning: open-PR search truncated at the {}-result search cap — coverage from PRs beyond it is unseen",
            SEARCH_PAGE_SIZE * SEARCH_MAX_PAGES
        );
    }
    Some(nodes)
}

/// Open issues NOT covered by any open PR. PURE: `covered` is the (repo, issue#) set an open PR's
/// native closing references link (see `covered_from_search_prs`).
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
    // Same paginated reader the `--queue` and `unvetted` gates use — one query, not a second
    // hand-rolled one. (It replaced a `first:50` single-page count that silently under-reported a
    // PR past 50 threads.) The ERROR semantics differ deliberately: an unreadable state here reads
    // as 0, because `worklist` answers "what should the PRODUCER do with this PR next?" and a
    // transient GraphQL failure must not manufacture a thread-resolution sweep with nothing to
    // resolve. Nothing is presented to a human off this value — `--queue` recomputes it fail-closed
    // before any PR reaches the approval queue.
    let threads = unresolved_threads(owner, repo, num).unwrap_or(0) as usize;
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
    // Match the note's SHA against the current head — the full oid OR its >=12-char prefix, so a
    // deploy-confirmed note that embedded a SHORT sha still counts as head-scoped. Guard on a
    // non-empty head so a missing headRefOid can never read as "deployed" (which would skip a
    // real redeploy and surface undeployed bytecode as ready).
    let head_short = if head.len() >= 12 { &head[..12] } else { head };
    let deploy_done_at_head = !head.is_empty()
        && trusted.iter().any(|c| {
            (c.contains("deploy") && (c.contains("SUCCESS") || c.contains("deploy-confirmed")))
                && (c.contains(head) || c.contains(head_short))
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
    // A human decision label beats any stale `ai:*` label — it BLOCKS routine producer action.
    let has_human_override = labels
        .iter()
        .any(|l| l == "human:reject" || l == "human:design" || l == "human:close-candidate");
    let sig = PrSignals {
        ci,
        merge_state: merge_state.clone(),
        unresolved_threads: threads,
        has_deploy_trigger,
        deploy_done_at_head,
        parked,
        ui_missing_screenshot,
        has_human_override,
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
        "humanOverride": has_human_override,
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

/// The uncovered-coverage result: the uncovered issues (repo, number) paired with a lookup from
/// (repo, number) to each issue's meta.
type UncoveredCoverage = (
    Vec<(String, u64)>,
    std::collections::HashMap<(String, u64), Value>,
);

/// Shared coverage computation: fetch open issues (org-scoped, WITH labels so callers can filter)
/// and the covered set from open PRs' native `closingIssuesReferences`, then return the uncovered
/// issues (no covering open PR) with their meta. `None` on a gh failure — callers MUST abort rather
/// than report a false-empty set. Both `uncovered-issues` and the `human-queue` producer-backlog
/// count read this ONE computation, so their coverage semantics can never drift.
fn coverage_uncovered() -> Option<UncoveredCoverage> {
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
        return None;
    };
    // open PRs + their NATIVE closing references (GraphQL). The REST `gh search prs` cannot
    // return `closingIssuesReferences`, and regexing title+body missed the URL and cross-repo
    // reference forms GitHub honors while over-counting title keywords GitHub ignores — the
    // native references are what actually auto-close at merge.
    let Some(pr_nodes) = search_open_prs_closing_refs() else {
        eprintln!(
            "error: open-PR closing-references search failed — aborting rather than report covered issues as uncovered"
        );
        return None;
    };
    let covered = covered_from_search_prs(&pr_nodes);

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

    Some((uncovered(&issues, &covered), meta))
}

/// True when an uncovered issue belongs to the PRODUCER's untouched backlog: NOT already flagged
/// `ai:close-candidate` (that is the human's close queue, surfaced separately) and carrying NO
/// `human:*` label (a human ruling is the human's inbox, not the producer's). The raw
/// `uncovered-issues` set does NOT apply these exclusions — the backlog is deliberately narrower.
fn is_producer_backlog(meta: &Value) -> bool {
    !meta
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter().any(|l| {
                let name = l.get("name").and_then(|n| n.as_str()).unwrap_or("");
                name == "ai:close-candidate" || name.starts_with("human:")
            })
        })
        .unwrap_or(false)
}

fn uncovered_issues_mode(json_out: bool) -> i32 {
    let Some((open, meta)) = coverage_uncovered() else {
        return 1;
    };
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
            work_dirs,
            dry_run,
            max_age_days,
        } => gc_clones_mode(&work_dirs, max_age_days, dry_run),
        Cmd::Gc {
            work_dirs,
            dry_run,
            max_age_days,
            no_clones,
            no_nix,
            nix_threshold,
        } => {
            let do_clones = !no_clones;
            let do_nix = !no_nix;
            if do_clones && work_dirs.is_empty() {
                eprintln!("error: gc needs <work-dir> unless --no-clones is given");
                std::process::exit(2);
            }
            gc_mode(
                &work_dirs,
                max_age_days,
                dry_run,
                do_clones,
                do_nix,
                nix_threshold,
            )
        }
        Cmd::RunMetrics {
            trace,
            run_id,
            role,
            model,
            exit_code,
            preflight_missing,
        } => run_metrics_mode(
            &trace,
            &RunIdentity {
                run_id: run_id.as_deref(),
                role: role.as_deref(),
                model: model.as_deref(),
            },
            exit_code,
            &preflight_missing,
        ),
        Cmd::Preflight => preflight_mode(),
        Cmd::ClosurePreflight { flake } => closure_preflight_mode(&flake),
        Cmd::ClosureRender { flake } => closure_render_mode(&flake),
        Cmd::ClosureSurface { flake } => closure_surface_mode(&flake),
        Cmd::RunTimings {
            out,
            trace,
            run_id,
            role,
            model,
        } => run_timings_mode(
            &out,
            &trace,
            &RunIdentity {
                run_id: run_id.as_deref(),
                role: role.as_deref(),
                model: model.as_deref(),
            },
        ),
        Cmd::TraceOutcome { trace, exit_code } => trace_outcome_mode(&trace, exit_code),
        Cmd::QueueHistoryLine { snapshot, ts } => queue_history_line_mode(snapshot.as_deref(), &ts),
        Cmd::DistillTrace => distill_trace_mode(),
        Cmd::UsageGate => usage_gate_mode(),
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
        Cmd::Unvetted {
            json,
            include_skipped,
            limit,
        } => unvetted_mode(json, include_skipped, limit),
        Cmd::Mcp { profile } => match McpProfile::parse(&profile) {
            Ok(p) => mcp_serve(p),
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        },
    };
    std::process::exit(code);
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use serde_json::json;

    /// An issue as `gh issue view --json` returns it, with one trusted producer flag.
    fn flagged_issue(labels: &[&str], flag_at: &str, reason: &str, extra: Vec<Value>) -> Value {
        let mut comments = vec![json!({
            "author": {"login": TRUSTED_AUTHOR},
            "createdAt": flag_at,
            "body": format!("🤖 ai:producer\nClose-candidate: {reason}"),
        })];
        comments.extend(extra);
        json!({
            "state": "OPEN",
            "labels": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            "comments": comments,
        })
    }

    fn vetter_cc_comment(flag_at: &str, verdict: &str) -> Value {
        json!({
            "author": {"login": TRUSTED_AUTHOR},
            "createdAt": "2026-07-26T10:00:00Z",
            "body": format!("🤖 ai:vetter\nReviewed close-candidate @{flag_at}: {verdict} — note"),
        })
    }

    // A human ruling on an ISSUE was invisible to the old check, which looked only for
    // `human:keep-open` (a label the org does not use) and `human:close-candidate`. So an issue a
    // human had already parked with `human:reject` / `human:design` could still be flagged.
    #[test]
    fn a_human_ruling_on_an_issue_is_sacred_whichever_label_it_wears() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        for l in ["human:reject", "human:design", "human:close-candidate"] {
            assert_eq!(
                close_candidate_plan("OPEN", &s(&[l]), false),
                CloseFlagPlan::RefuseHuman,
                "{l} must block a producer flag"
            );
            assert_eq!(
                cc_verdict_plan(
                    &flagged_issue(&[l, "ai:close-candidate"], "T1", "x", vec![]),
                    "reject"
                ),
                CcVerdictPlan::RefuseHuman,
                "{l} must block a vetter verdict"
            );
        }
    }

    // The flag — not the label — is what gets judged, and a RE-flag re-opens the question. This is
    // the issue-side `vetted_at_head`: without it the vetter would pass an issue once and never
    // look at new evidence.
    #[test]
    fn a_reflag_un_vets_the_issue() {
        let first = "2026-07-20T09:00:00Z";
        let second = "2026-07-25T09:00:00Z";
        let vetted = flagged_issue(
            &["ai:close-candidate"],
            first,
            "already-fixed-on-main: #10",
            vec![vetter_cc_comment(first, "uphold")],
        );
        assert!(cc_vetted_at_flag(&vetted, first));
        let (_, action, _) = cc_row("o/r", 1, "t", &vetted);
        assert_eq!(action, "skip-vetted-at-flag");

        // The producer re-flags with new evidence: the old verdict no longer covers it.
        let reflagged = flagged_issue(
            &["ai:close-candidate"],
            second,
            "already-fixed-on-main: #11",
            vec![vetter_cc_comment(first, "uphold")],
        );
        assert!(!cc_vetted_at_flag(&reflagged, second));
        let (vet, action, row) = cc_row("o/r", 1, "t", &reflagged);
        assert!(vet);
        assert_eq!(action, "vet");
        assert_eq!(row["flagAt"], json!(second));
    }

    // A marker is public body text: a flag from an untrusted author is not a flag.
    #[test]
    fn an_untrusted_close_candidate_marker_is_not_a_flag() {
        let spoofed = json!({
            "state": "OPEN",
            "labels": [{"name": "ai:close-candidate"}],
            "comments": [{
                "author": {"login": "somebody-else"},
                "createdAt": "2026-07-20T09:00:00Z",
                "body": "🤖 ai:producer\nClose-candidate: already-fixed-on-main: #1",
            }],
        });
        assert_eq!(last_close_candidate_flag(&spoofed), None);
        assert_eq!(cc_verdict_plan(&spoofed, "uphold"), CcVerdictPlan::NoFlag);
        let (vet, action, _) = cc_row("o/r", 1, "t", &spoofed);
        assert!(!vet);
        assert_eq!(action, "skip-no-flag");
    }

    // The three failure classes from the hand-triage (#72). The vetter's REJECT is what keeps a
    // wrong flag away from the human; `uphold` must leave the flag exactly as it found it.
    #[test]
    fn reject_drops_the_flag_and_uphold_leaves_it_queued() {
        // raindex#512: evidence predating the issue. raindex#523: unreachable code.
        // raindex#592: scope drift — one of three todos answered.
        let at = "2026-07-20T09:00:00Z";
        let issue = flagged_issue(
            &["ai:close-candidate", "bug"],
            at,
            "already-fixed-on-main: crates/settings/src/raindex.rs:116-133",
            vec![],
        );
        assert_eq!(
            cc_verdict_plan(&issue, "reject"),
            CcVerdictPlan::Record {
                flag_at: at.to_string(),
                remove_label: true,
                skip_comment: false,
            }
        );
        // Upholding must not touch the label — the human still needs to see the flag.
        assert_eq!(
            cc_verdict_plan(&issue, "uphold"),
            CcVerdictPlan::Record {
                flag_at: at.to_string(),
                remove_label: false,
                skip_comment: false,
            }
        );
        // Re-recording the SAME verdict at the SAME flag is a no-op comment.
        let already = flagged_issue(
            &["ai:close-candidate"],
            at,
            "already-fixed-on-main: #10",
            vec![vetter_cc_comment(at, "reject")],
        );
        match cc_verdict_plan(&already, "reject") {
            CcVerdictPlan::Record { skip_comment, .. } => assert!(skip_comment),
            other => panic!("expected Record, got {other:?}"),
        }
        // ...but a DIFFERENT verdict at the same flag still posts.
        match cc_verdict_plan(&already, "uphold") {
            CcVerdictPlan::Record { skip_comment, .. } => assert!(!skip_comment),
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn cc_verdict_comment_pins_the_flag_it_judged() {
        assert_eq!(
            cc_verdict_comment("2026-07-20T09:00:00Z", "reject", "evidence predates the issue"),
            "🤖 ai:vetter\nReviewed close-candidate @2026-07-20T09:00:00Z: reject — evidence predates the issue"
        );
        assert_eq!(
            cc_verdict_comment("T", "uphold", "   "),
            "🤖 ai:vetter\nReviewed close-candidate @T: uphold"
        );
    }

    // The close-candidate queue searches ISSUE urls, and `pr_slug` rejects those BY DESIGN. Reusing
    // it there parsed every row to None, so the whole queue silently stayed empty and the feature
    // did nothing at all.
    #[test]
    fn issue_slug_parses_issue_urls_where_pr_slug_deliberately_will_not() {
        assert_eq!(
            issue_slug("https://github.com/rainlanguage/raindex/issues/512").as_deref(),
            Some("rainlanguage/raindex")
        );
        assert_eq!(
            pr_slug("https://github.com/rainlanguage/raindex/issues/512"),
            None
        );
        // ...and the reverse: an issue slug is not a PR slug.
        assert_eq!(issue_slug("https://github.com/o/r/pull/1"), None);
        assert_eq!(issue_slug("https://example.com/o/r/issues/1"), None);
        assert_eq!(issue_slug("https://github.com/r/issues/1"), None);
        assert_eq!(issue_slug(""), None);
    }

    // The dashboard's boxes are CLICK-THROUGH: each state renders a count and, when clicked, lists
    // the issues from the top-level array of the SAME name. So both must exist, carry the shape
    // `closeCandidateIssues` / `uncoveredIssues` already use, and agree — a "5" that lists three
    // issues is the drift worth pinning.
    #[test]
    fn cc_item_arrays_are_populated_and_agree_with_their_counts() {
        let row = |repo: &str, num: u64, title: &str, action: &str| {
            json!({"issue": format!("{repo}#{num}"), "repo": repo, "number": num,
                   "title": title, "action": action})
        };
        let doc = json!({
            "issues": [
                row("rainlanguage/raindex", 512, "orderbooks should fallback", "vet"),
                row("rainlanguage/raindex", 592, "Fix inverted IO ratio", "vet"),
            ],
            "skipped": [
                row("rainlanguage/raindex", 523, "nothing visual happens", "skip-vetted-at-flag"),
                // Neither of these is UPHELD: one is a human ruling, one has no flag to judge.
                row("rainlanguage/raindex", 184, "frontmatter lint", "skip-human-decided"),
                row("rainlanguage/raindex", 999, "no flag", "skip-no-flag"),
            ],
        });
        let (unvetted, upheld) = cc_item_arrays(&doc);

        // Populated, and each item carries EXACTLY the generic issue-item shape.
        assert_eq!(unvetted.len(), 2);
        assert_eq!(upheld.len(), 1);
        assert_eq!(
            unvetted[0],
            json!({"repo": "rainlanguage/raindex", "number": 512, "title": "orderbooks should fallback"})
        );
        assert_eq!(
            upheld[0],
            json!({"repo": "rainlanguage/raindex", "number": 523, "title": "nothing visual happens"})
        );
        // Only `skip-vetted-at-flag` is upheld — a human ruling or a missing flag is neither.
        for r in &upheld {
            assert_ne!(r["number"], json!(184));
            assert_ne!(r["number"], json!(999));
        }

        // A doc with no skipped rows (include_skipped omitted) yields an EMPTY upheld array, never
        // a count without items.
        let (u2, up2) = cc_item_arrays(&json!({"issues": []}));
        assert!(u2.is_empty() && up2.is_empty());
    }

    // The count a state box shows and the list it opens must be the same data. Asserting
    // `len() == len()` at the call site is a TAUTOLOGY that a decoupled emission survives (it did,
    // when the count was computed separately from the array), so what gets pinned is the pairing
    // itself: whatever array comes out, the count is ITS length.
    #[test]
    fn an_issue_state_count_is_always_its_own_arrays_length() {
        for n in [0usize, 1, 5] {
            let items: Vec<Value> = (0..n)
                .map(|i| json!({"repo": "o/r", "number": i, "title": "t"}))
                .collect();
            let (arr, count) = issue_state_pair(items);
            assert_eq!(count, n);
            assert_eq!(
                arr.as_array().expect("emitted as a JSON array").len(),
                count,
                "a state box showing {count} must list exactly {count} issues"
            );
        }
    }

    #[test]
    fn flag_reason_is_the_claim_without_the_marker() {
        assert_eq!(
            flag_reason("🤖 ai:producer\nClose-candidate: invalid: premise obsolete"),
            "invalid: premise obsolete"
        );
        assert_eq!(flag_reason("🤖 ai:producer\nsomething else"), "");
    }

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
    fn already_fixed_anchor_requires_something_datable() {
        // The real shape that let live issues through: a bare file:line (raindex#512 cited
        // crates/settings/src/raindex.rs:116-133, code that predated the issue by two weeks).
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: crates/settings/src/raindex.rs:116-133"),
            FixAnchor::Missing
        );
        // The other categories are judgements, not landings — never gated.
        assert_eq!(
            already_fixed_anchor("invalid: the premise is obsolete, no tauri.conf.json exists"),
            FixAnchor::NotApplicable
        );
        assert_eq!(
            already_fixed_anchor("duplicate: of #123"),
            FixAnchor::NotApplicable
        );
        assert_eq!(
            already_fixed_anchor("wont-fix: superseded by the registry flow"),
            FixAnchor::NotApplicable
        );
        // Datable anchors are extracted.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: fixed by PR #2420"),
            FixAnchor::Pr("2420".into())
        );
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: 2a319034 removed the tauri app"),
            FixAnchor::Commit("2a319034".into())
        );
        // A sha alongside a file:line still counts — the sha is what gets date-checked.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: impls.rs:205 via a665ea9f7"),
            FixAnchor::Commit("a665ea9f7".into())
        );
        // Prose must not be mistaken for a sha: short hex-ish words and all-alpha words are not.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: added a decade ago, see the code"),
            FixAnchor::Missing
        );
        // `#` with no digits after it is not a PR reference.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: see foo#bar in the docs"),
            FixAnchor::Missing
        );
        // The forms a producer actually types must all resolve — `PR#123` has an alphanumeric
        // before the `#`, and rejecting it would refuse evidence in the format the prompt asks for.
        for reason in [
            "already-fixed-on-main: PR#2420",
            "already-fixed-on-main: fixed in rainlanguage/raindex#2420",
            "already-fixed-on-main: see #2420",
        ] {
            assert_eq!(
                already_fixed_anchor(reason),
                FixAnchor::Pr("2420".into()),
                "{reason}"
            );
        }
        // An all-letter hex sha is still a sha.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: deadbeef fixed it"),
            FixAnchor::Commit("deadbeef".into())
        );
        // A bare number is NOT a sha — a date or an id must report "no usable anchor", not send
        // the caller to `gh api .../commits/20240401` and surface a date-resolution error.
        for reason in [
            "already-fixed-on-main: landed 20240401",
            "already-fixed-on-main: see build 1234567",
        ] {
            assert_eq!(already_fixed_anchor(reason), FixAnchor::Missing, "{reason}");
        }
        // A reason typed with leading whitespace is the same claim — without the trim it reads as
        // "not an already-fixed claim" and the gate is skipped entirely, which is the fail-OPEN
        // direction (the flag lands unchecked).
        assert_eq!(
            already_fixed_anchor("  \talready-fixed-on-main: fixed by #2420"),
            FixAnchor::Pr("2420".into())
        );
        // The FIRST `#` may be prose; a real reference later in the reason still counts.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: not foo#bar — fixed by #2420"),
            FixAnchor::Pr("2420".into())
        );
        // Sha length boundaries: 7 (git's short sha) and 40 (a full sha) are both in, 41 is not —
        // past 40 the hex run is no longer a sha and dating it would be resolving a coincidence.
        assert_eq!(
            already_fixed_anchor("already-fixed-on-main: a665ea9 fixed it"),
            FixAnchor::Commit("a665ea9".into())
        );
        let full = "a665ea9f7c3b21d04e8f6a5b9c0d1e2f3a4b5c6d";
        assert_eq!(full.len(), 40);
        assert_eq!(
            already_fixed_anchor(&format!("already-fixed-on-main: {full} fixed it")),
            FixAnchor::Commit(full.into())
        );
        assert_eq!(
            already_fixed_anchor(&format!("already-fixed-on-main: {full}a fixed it")),
            FixAnchor::Missing
        );
    }

    #[test]
    fn landed_after_filed_compares_iso_instants() {
        // raindex#512: filed 2024-04-01, cited fallback landed 2024-03-16 -> predates, must fail.
        assert_eq!(
            landed_after_filed("2024-03-16T09:00:00Z", "2024-04-01T11:06:35Z"),
            Some(false)
        );
        // raindex#529: filed 2024-04-03, the runs-default landed later -> genuine fix.
        assert_eq!(
            landed_after_filed("2024-06-01T00:00:00Z", "2024-04-03T10:00:00Z"),
            Some(true)
        );
        // Same instant is not "after" — a fix cannot be its own report.
        assert_eq!(
            landed_after_filed("2024-04-01T11:06:35Z", "2024-04-01T11:06:35Z"),
            Some(false)
        );
        // Unparseable -> None, so the caller fails closed instead of guessing.
        assert_eq!(landed_after_filed("", "2024-04-01T11:06:35Z"), None);
        assert_eq!(
            landed_after_filed("2024-04-01", "2024-04-01T11:06:35Z"),
            None
        );
        // BOTH sides are validated, not just the landing date — an unusable `createdAt` must not
        // be silently compared against a good landing date.
        assert_eq!(
            landed_after_filed("2024-06-01T00:00:00Z", "2024-04-01"),
            None
        );
        // A zoneless instant (19 chars) is not the `…Z` form GitHub emits — fail closed rather
        // than compare two strings that are not on the same clock.
        assert_eq!(
            landed_after_filed("2024-04-01T11:06:35", "2024-04-01T11:06:35Z"),
            None
        );
        // Long enough but not a date at all: without the `T` check this compares as plain text
        // and returns a confident, meaningless verdict.
        assert_eq!(
            landed_after_filed("not a date at all!!!!", "2024-04-01T11:06:35Z"),
            None
        );
        assert_eq!(
            landed_after_filed("2024-06-01T00:00:00Z", "no idea when this was"),
            None
        );
    }

    #[test]
    fn recency_gate_blocks_predating_evidence_and_missing_dates() {
        // No createdAt -> cannot check -> fail closed (1), never silently allow.
        assert_eq!(
            already_fixed_recency_gate("o/r", "1", "already-fixed-on-main: #7", &json!({})),
            1
        );
        // Bare file:line -> unsupported claim (4).
        assert_eq!(
            already_fixed_recency_gate(
                "o/r",
                "1",
                "already-fixed-on-main: src/foo.rs:10",
                &json!({"createdAt": "2024-04-01T11:06:35Z"})
            ),
            4
        );
        // A non-already-fixed category is never gated, even with no createdAt.
        assert_eq!(
            already_fixed_recency_gate("o/r", "1", "invalid: premise obsolete", &json!({})),
            0
        );
    }

    #[test]
    fn recency_exit_code_admits_only_a_fix_that_postdates_the_report() {
        // The PASS arm. Without it a gate that refuses every flag is indistinguishable from one
        // that works — the whole `already-fixed-on-main` category would just stop, silently.
        assert_eq!(
            recency_exit_code(
                "o/r",
                "529",
                "PR #2420",
                "2024-06-01T00:00:00Z",
                "2024-04-03T10:00:00Z"
            ),
            0
        );
        // raindex#512's shape: the cited change landed BEFORE the bug was reported -> unsupported
        // claim (4). This is the defect #71 exists for; 0 here is a human closing a live bug.
        assert_eq!(
            recency_exit_code(
                "o/r",
                "512",
                "commit 2a319034",
                "2024-03-16T09:00:00Z",
                "2024-04-01T11:06:35Z"
            ),
            4
        );
        // Same instant is not "after" — a fix cannot be its own report.
        assert_eq!(
            recency_exit_code(
                "o/r",
                "1",
                "PR #7",
                "2024-04-01T11:06:35Z",
                "2024-04-01T11:06:35Z"
            ),
            4
        );
        // No date resolved (an UNMERGED PR, or the lookup failed) -> 1, never 0. The producer
        // citing an open PR as the landed fix is the common case here.
        assert_eq!(
            recency_exit_code("o/r", "1", "PR #2420", "", "2024-04-01T11:06:35Z"),
            1
        );
        // A date that will not parse is also fail-closed, and distinct from 4: nothing was
        // disproved, it just could not be checked.
        assert_eq!(
            recency_exit_code(
                "o/r",
                "1",
                "commit deadbeef",
                "2024-06-01",
                "2024-04-01T11:06:35Z"
            ),
            1
        );
        assert_eq!(
            recency_exit_code(
                "o/r",
                "1",
                "commit deadbeef",
                "2024-06-01T00:00:00Z",
                "2024"
            ),
            1
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
            open_threads: 0,
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
        c.open_threads = 4;
        let out = render_queue(&rows, &c, 0);
        assert!(out.contains("  unscored  r#2  "), "unscored:\n{out}");
        assert!(out.contains("1 fetch-error"));
        assert!(out.contains("1 excluded (draft/human-override)"));
        assert!(out.contains("4 open-threads"), "open-threads note:\n{out}");
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
mod open_threads_tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    fn page(nodes: Value, has_next: bool, cursor: &str) -> Value {
        json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "nodes": nodes,
            "pageInfo": {"hasNextPage": has_next, "endCursor": cursor}
        }}}}})
    }

    // T1: unresolved threads are counted by the typed isResolved field, resolved ones excluded.
    #[test]
    fn count_mixed_resolved_unresolved() {
        let v = page(
            json!([{"isResolved": false}, {"isResolved": true}, {"isResolved": false}]),
            false,
            "",
        );
        assert_eq!(count_unresolved_page(&v), Some((2, None)));
    }

    // T2: zero threads is a verified-clean Some(0), distinct from unknown.
    #[test]
    fn count_empty_nodes_is_zero() {
        assert_eq!(
            count_unresolved_page(&page(json!([]), false, "")),
            Some((0, None))
        );
    }

    // T3: a further page propagates its cursor so pagination can't silently truncate.
    #[test]
    fn count_propagates_next_cursor() {
        let v = page(json!([{"isResolved": false}]), true, "CUR");
        assert_eq!(
            count_unresolved_page(&v),
            Some((1, Some("CUR".to_string())))
        );
    }

    // T4: malformed responses (missing pullRequest, non-array nodes, node without isResolved,
    // GraphQL error shape) are None — unknown, never a silent 0.
    #[test]
    fn count_malformed_is_none() {
        assert_eq!(
            count_unresolved_page(&json!({"data": {"repository": null}})),
            None
        );
        assert_eq!(
            count_unresolved_page(&json!({"errors": [{"message": "boom"}]})),
            None
        );
        let bad_nodes = json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "nodes": "nope", "pageInfo": {"hasNextPage": false, "endCursor": ""}}}}}});
        assert_eq!(count_unresolved_page(&bad_nodes), None);
        let bad_node = page(json!([{"resolved": true}]), false, "");
        assert_eq!(count_unresolved_page(&bad_node), None);
    }

    // T5: the three thread states route three different ways — clean is the ONLY one presented,
    // and "could not read" is its own outcome, never folded into clean or into dirty.
    #[test]
    fn queue_routing_is_fail_closed_and_three_way() {
        assert_eq!(thread_route(Some(0)), ThreadRoute::Present);
        assert_eq!(thread_route(Some(1)), ThreadRoute::OpenThreads);
        assert_eq!(thread_route(Some(9)), ThreadRoute::OpenThreads);
        assert_eq!(thread_route(None), ThreadRoute::FetchError);
    }

    // T6: PAGING — the total is every page summed, and each page after the first is fetched with
    // the PREVIOUS page's endCursor. A reader that stopped at page 1 would report 1, not 3.
    #[test]
    fn total_sums_every_page_and_resumes_from_the_cursor() {
        let seen: RefCell<Vec<Option<String>>> = RefCell::new(Vec::new());
        let total = total_unresolved(|cursor| {
            seen.borrow_mut().push(cursor.map(String::from));
            match cursor {
                None => Some(page(json!([{"isResolved": false}]), true, "C1")),
                Some("C1") => Some(page(
                    json!([{"isResolved": false}, {"isResolved": true}, {"isResolved": false}]),
                    false,
                    "",
                )),
                Some(other) => panic!("unexpected cursor {other}"),
            }
        });
        assert_eq!(total, Some(3), "both pages must be counted");
        assert_eq!(
            *seen.borrow(),
            vec![None, Some("C1".to_string())],
            "page 2 must resume from page 1's endCursor"
        );
    }

    // T7: a page that cannot be read mid-pagination yields None — NEVER the partial total already
    // accumulated, which would be a truncated count presented as a whole one.
    #[test]
    fn total_is_none_when_a_later_page_is_unreadable() {
        let total = total_unresolved(|cursor| match cursor {
            None => Some(page(json!([{"isResolved": false}]), true, "C1")),
            Some(_) => None,
        });
        assert_eq!(total, None);
    }

    // T8: a cursor that never terminates stops at the page cap and reports UNKNOWN, not the
    // partial sum — an unbounded loop and a truncated total are both unacceptable.
    #[test]
    fn total_stops_at_the_page_cap_without_reporting_a_partial() {
        let calls = Cell::new(0usize);
        let total = total_unresolved(|_| {
            calls.set(calls.get() + 1);
            Some(page(json!([{"isResolved": false}]), true, "SAME"))
        });
        assert_eq!(
            total, None,
            "a non-terminating cursor must not yield a total"
        );
        assert_eq!(calls.get(), MAX_THREAD_PAGES, "paging must be bounded");
    }

    // --- the VETTER's state-load gate (`unvetted`), issue #1 requirement 2 ------------------------

    fn vet_row() -> (VetAction, u8, Value) {
        (VetAction::Vet, 0, json!({"pr": "o/r#1", "action": "vet"}))
    }

    // T9: a PR with an unresolved thread is NOT offered to the vetter, so it can never be given a
    // `ready` verdict while a thread is open.
    #[test]
    fn vet_gate_excludes_a_pr_with_an_unresolved_thread() {
        let (action, _, row) = gate_open_threads(vet_row(), || Some(1));
        assert_eq!(action, VetAction::SkipOpenThreads);
        assert_eq!(row["action"], json!("skip-open-threads"));
        assert_eq!(row["unresolvedThreads"], json!(1));
    }

    // T10: a PR with a VERIFIED zero passes through untouched — the gate must not withhold clean
    // PRs, or vetting stops entirely.
    #[test]
    fn vet_gate_passes_a_pr_with_zero_unresolved_threads() {
        let (action, prio, row) = gate_open_threads(vet_row(), || Some(0));
        assert_eq!(action, VetAction::Vet);
        assert_eq!(prio, 0);
        assert_eq!(row["action"], json!("vet"));
        assert_eq!(row["unresolvedThreads"], json!(0));
    }

    // T11: an unreadable thread state is fail-closed (not vetted), and stays DISTINGUISHABLE from
    // a verified zero on the row.
    #[test]
    fn vet_gate_fails_closed_on_unknown_thread_state() {
        let (action, _, row) = gate_open_threads(vet_row(), || None);
        assert_eq!(action, VetAction::SkipOpenThreads);
        assert_eq!(row["unresolvedThreads"], json!(null));
    }

    // T12: a row that ALREADY skips costs no GraphQL round-trip — the gate only asks about PRs
    // whose answer could change the outcome.
    #[test]
    fn vet_gate_does_not_query_an_already_skipped_row() {
        for skip in [
            VetAction::SkipHuman,
            VetAction::SkipDraft,
            VetAction::SkipVetted,
        ] {
            let called = Cell::new(false);
            let (action, _, row) = gate_open_threads(
                (skip, 4, json!({"pr": "o/r#1", "action": skip.as_str()})),
                || {
                    called.set(true);
                    Some(7)
                },
            );
            assert!(!called.get(), "{skip:?} must not trigger a thread query");
            assert_eq!(action, skip);
            assert_eq!(row.get("unresolvedThreads"), None);
        }
    }

    // T13: the state-load counts an open-threads skip as ITS OWN reason — folding it into
    // `skipVettedAtHead` would report un-vetted PRs as already vetted.
    #[test]
    fn doc_counts_the_open_threads_skip_separately() {
        let rows = vec![
            (VetAction::Vet, 0, json!({"pr": "o/r#1", "action": "vet"})),
            (
                VetAction::SkipOpenThreads,
                0,
                json!({"pr": "o/r#2", "action": "skip-open-threads"}),
            ),
            (
                VetAction::SkipVetted,
                4,
                json!({"pr": "o/r#3", "action": "skip-vetted-at-head"}),
            ),
        ];
        let doc = unvetted_doc(&rows, false, None);
        assert_eq!(doc["counts"]["vet"], json!(1));
        assert_eq!(doc["counts"]["skipOpenThreads"], json!(1));
        assert_eq!(doc["counts"]["skipVettedAtHead"], json!(1));
        let listed: Vec<&str> = doc["prs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["pr"].as_str().unwrap())
            .collect();
        assert_eq!(
            listed,
            vec!["o/r#1"],
            "a gated PR is not handed to the vetter"
        );
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

/// The boot / ttl split (#84).
///
/// `startupMs` fused two costs with nothing in common — nix resolving and exec'ing the flake
/// output, and the model orienting itself across a tool surface — so a move in it could not be
/// attributed to either. These pin the two halves apart, and pin `startupMs` in place while they
/// are added.
#[cfg(test)]
mod startup_split_tests {
    use super::{
        final_record, is_mutation_tool, partial_record, run_metrics, RunIdentity, RunMetrics,
        StartupPhase, StartupProbe, ToolingReport, TraceOutcome, STAGE_FINAL,
    };
    use serde_json::{json, Value};

    /// An assistant event carrying one `tool_use`, WITH a timestamp. The modern harness stamps
    /// assistant events; that stamp is the only thing that makes boot and ttl measurable.
    fn tool_at(ts: &str, name: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"message":{"content":[
            {"type":"tool_use","name":name,"input":{"command":""}}]}})
        .to_string()
    }
    /// An assistant event with no `tool_use` — the model's opening text. This is normally a run's
    /// first timestamped event, so it is what `bootMs` measures FROM.
    fn text_at(ts: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"message":{"content":[
            {"type":"text","text":"starting the run"}]}})
        .to_string()
    }
    /// A `user` event: a tool RESULT.
    fn result_at(ts: &str) -> String {
        json!({"type":"user","timestamp":ts,"message":{"content":[]}}).to_string()
    }
    /// An assistant `tool_use` with NO timestamp — the shape older harnesses emitted.
    fn tool_untimed(name: &str, cmd: &str) -> String {
        json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":name,"input":{"command":cmd}}]}})
        .to_string()
    }
    fn ev(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }
    fn id() -> RunIdentity<'static> {
        RunIdentity {
            run_id: Some("20260728T053610Z"),
            role: Some("vetter"),
            model: Some("claude-fable-5"),
        }
    }

    /// The live vetter run `review-runs/20260728T053610Z.jsonl`, reduced to its five load-bearing
    /// events. Every number asserted below was measured off that real trace.
    fn vetter_20260728t053610z() -> String {
        [
            text_at("2026-07-28T05:36:13.214Z"), // run's first timestamped event
            tool_at("2026-07-28T05:36:14.339Z", "mcp__fsm__unvetted"), // FIRST tool call
            result_at("2026-07-28T05:38:30.435Z"), // its result — 136 s later
            tool_at("2026-07-28T05:41:11.355Z", "mcp__fsm__record_verdict"), // FIRST verdict
            result_at("2026-07-28T05:41:16.994Z"), // the verdict's result
        ]
        .join("\n")
    }

    #[test]
    fn boot_is_the_run_start_to_the_first_tool_call() {
        let m = run_metrics(&vetter_20260728t053610z());
        // Hand-measured on the real trace: 1.125 s. Launch overhead is sub-second — whatever the
        // flake conversion cost, it did not cost boot.
        assert_eq!(m.boot_ms, Some(1125));
    }

    #[test]
    fn ttl_is_the_first_tool_call_to_the_first_productive_call() {
        let m = run_metrics(&vetter_20260728t053610z());
        // 297.0 s of orientation before the first verdict, at tool call 17 (index 16).
        assert_eq!(m.ttl_ms, Some(297_016));
        assert_eq!(m.first_mutation_index, Some(1));
    }

    /// The reason the split is worth having: on this run ttl is 264× boot. One number that either
    /// can wreck is a number you cannot act on.
    #[test]
    fn ttl_dominates_boot_on_a_real_vetter_run() {
        let m = run_metrics(&vetter_20260728t053610z());
        let (boot, ttl) = (m.boot_ms.unwrap(), m.ttl_ms.unwrap());
        assert!(
            ttl > boot * 100,
            "orientation should dwarf launch on this run: boot {boot}ms vs ttl {ttl}ms"
        );
    }

    /// `startupMs` is FROZEN. Its anchor is the first tool RESULT, so it opens 137 s after the run
    /// actually started and misses the first call's own latency entirely — a real flaw, but the
    /// one every committed record was written under. Re-anchoring it would put a step in the
    /// dashboard series that no run ever experienced, so it keeps its old value and boot+ttl are
    /// added beside it.
    #[test]
    fn startup_ms_keeps_its_first_result_anchor_and_is_not_boot_plus_ttl() {
        let m = run_metrics(&vetter_20260728t053610z());
        assert_eq!(m.startup_ms, Some(166_559));
        let fused = m.boot_ms.unwrap() + m.ttl_ms.unwrap();
        assert_eq!(fused, 298_141);
        assert_ne!(
            m.startup_ms,
            Some(fused),
            "startupMs must NOT be silently redefined as bootMs + ttlMs"
        );
    }

    /// The vetter has no Bash at all (#52): `record_verdict` IS its mutation. A Bash-only detector
    /// calls every modern vetter run "never mutated" and loses the ttl anchor with it.
    #[test]
    fn the_vetters_mcp_writes_are_mutations() {
        assert!(is_mutation_tool("mcp__fsm__record_verdict", &json!({})));
        assert!(is_mutation_tool(
            "mcp__fsm__record_close_candidate_verdict",
            &json!({})
        ));
        // …and its READS are not.
        assert!(!is_mutation_tool("mcp__fsm__unvetted", &json!({})));
        assert!(!is_mutation_tool("mcp__fsm__pr_context", &json!({})));
        assert!(!is_mutation_tool("mcp__fsm__pr_checkout", &json!({})));
        assert!(!is_mutation_tool("mcp__fsm__clone_release", &json!({})));
    }

    #[test]
    fn a_bashless_vetter_run_still_finds_its_first_productive_call() {
        let trace = [
            text_at("2026-07-28T05:00:00.000Z"),
            tool_at("2026-07-28T05:00:01.000Z", "mcp__fsm__unvetted"),
            result_at("2026-07-28T05:00:02.000Z"),
            tool_at("2026-07-28T05:00:03.000Z", "mcp__fsm__pr_context"),
            result_at("2026-07-28T05:00:04.000Z"),
            tool_at("2026-07-28T05:00:09.000Z", "mcp__fsm__record_verdict"),
            result_at("2026-07-28T05:00:10.000Z"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.first_mutation_index, Some(2));
        assert_eq!(m.startup_tool_calls, 2);
        assert_eq!(m.ttl_ms, Some(8000));
    }

    /// Older harnesses stamped only `user` events. boot and ttl then have no anchor and must be
    /// null — while `startupMs`, which only ever needed `user` timestamps, still computes. That is
    /// what keeps a re-derivation of the archived traces honest instead of inventing zeroes.
    #[test]
    fn boot_and_ttl_are_null_without_assistant_timestamps() {
        let trace = [
            tool_untimed("Bash", "gh search prs"),
            result_at("2026-07-05T09:00:00.000Z"),
            tool_untimed("Bash", "gh pr create -R x"),
            result_at("2026-07-05T09:00:12.500Z"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.boot_ms, None);
        assert_eq!(m.ttl_ms, None);
        assert_eq!(m.startup_ms, Some(12_500));
    }

    /// An untimestamped FIRST call must not hand the anchor to a later, timestamped one: that
    /// would report a plausible, wrong boot instead of an honest gap.
    #[test]
    fn an_untimestamped_first_call_does_not_promote_a_later_call_to_the_anchor() {
        let trace = [
            text_at("2026-07-05T09:00:00.000Z"),
            tool_untimed("Bash", "gh search prs"), // FIRST call, no stamp
            tool_at("2026-07-05T09:00:10.000Z", "Read"),
            tool_untimed("Bash", "gh pr create -R x"),
        ]
        .join("\n");
        let m = run_metrics(&trace);
        assert_eq!(m.boot_ms, None, "10s to the SECOND call is not boot");
        assert_eq!(m.ttl_ms, None);
    }

    // ---- when each number becomes KNOWN --------------------------------------------------

    #[test]
    fn boot_is_known_at_the_first_tool_call_and_ttl_at_the_first_productive_one() {
        let mut p = StartupProbe::default();
        assert_eq!(p.observe(&ev(&text_at("2026-07-28T05:36:13.214Z"))), vec![]);
        assert_eq!(
            p.observe(&ev(&tool_at(
                "2026-07-28T05:36:14.339Z",
                "mcp__fsm__unvetted"
            ))),
            vec![StartupPhase::Boot],
            "boot is knowable the instant the first tool call is seen"
        );
        assert_eq!(p.boot_ms(), Some(1125));
        assert_eq!(
            p.observe(&ev(&result_at("2026-07-28T05:38:30.435Z"))),
            vec![]
        );
        assert_eq!(
            p.observe(&ev(&tool_at(
                "2026-07-28T05:41:11.355Z",
                "mcp__fsm__record_verdict"
            ))),
            vec![StartupPhase::Ttl],
            "ttl is knowable at the productive CALL, not at its result"
        );
        assert_eq!(p.ttl_ms(), Some(297_016));
    }

    /// A run whose very first call is already productive knows both numbers on one event.
    #[test]
    fn both_phases_fire_when_the_first_call_is_productive() {
        // A first call that is NOT productive settles boot alone — ttl stays open.
        let mut p = StartupProbe::default();
        p.observe(&ev(&text_at("2026-07-05T09:00:00.000Z")));
        assert_eq!(
            p.observe(&ev(&tool_at("2026-07-05T09:00:00.500Z", "Bash"))),
            vec![StartupPhase::Boot],
            "a Bash call with no mutating command settles boot but not ttl"
        );
        assert_eq!(p.ttl_ms(), None);

        let mut p = StartupProbe::default();
        p.observe(&ev(&text_at("2026-07-05T09:00:00.000Z")));
        let productive = json!({"type":"assistant","timestamp":"2026-07-05T09:00:00.500Z",
            "message":{"content":[{"type":"tool_use","name":"Bash",
                "input":{"command":"gh pr create -R x"}}]}});
        assert_eq!(
            p.observe(&productive),
            vec![StartupPhase::Boot, StartupPhase::Ttl]
        );
        assert_eq!(p.boot_ms(), Some(500));
        assert_eq!(p.ttl_ms(), Some(0));
    }

    /// A phase fires ONCE. A second verdict must not append a second `ttl` record.
    #[test]
    fn each_phase_fires_exactly_once() {
        let mut p = StartupProbe::default();
        p.observe(&ev(&text_at("2026-07-05T09:00:00.000Z")));
        p.observe(&ev(&tool_at("2026-07-05T09:00:01.000Z", "Read")));
        p.observe(&ev(&tool_at(
            "2026-07-05T09:00:02.000Z",
            "mcp__fsm__record_verdict",
        )));
        assert_eq!(
            p.observe(&ev(&tool_at(
                "2026-07-05T09:00:03.000Z",
                "mcp__fsm__record_verdict"
            ))),
            vec![],
            "the SECOND verdict is not a second first-productive-act"
        );
        assert_eq!(p.ttl_ms(), Some(1000));
    }

    // ---- record shapes -------------------------------------------------------------------

    /// A partial carries what is KNOWN and nothing else. The omissions are the contract: a run
    /// still going has no `toolCalls`, no `startupPct`, no `durationMs`, no `outcome`, and — since
    /// `startupMs` closes on the mutation's RESULT — no `startupMs` either.
    #[test]
    fn the_boot_partial_carries_boot_and_nothing_it_cannot_know() {
        let mut p = StartupProbe::default();
        p.observe(&ev(&text_at("2026-07-28T05:36:13.214Z")));
        p.observe(&ev(&tool_at(
            "2026-07-28T05:36:14.339Z",
            "mcp__fsm__unvetted",
        )));
        let doc = partial_record(&p, StartupPhase::Boot, "/t.jsonl", &id());
        assert_eq!(doc["stage"], "boot");
        assert_eq!(doc["bootMs"], 1125);
        assert_eq!(doc["trace"], "/t.jsonl");
        assert_eq!(doc["runId"], "20260728T053610Z");
        assert_eq!(doc["role"], "vetter");
        assert_eq!(doc["model"], "claude-fable-5");
        for absent in [
            "ttlMs",
            "startupMs",
            "toolCalls",
            "startupPct",
            "durationMs",
            "outcome",
            "exitCode",
        ] {
            assert!(
                doc.get(absent).is_none(),
                "a boot partial cannot know {absent}"
            );
        }
    }

    #[test]
    fn the_ttl_partial_carries_both_timings_and_the_call_counts_it_has() {
        let mut p = StartupProbe::default();
        for line in vetter_20260728t053610z().lines().take(4) {
            p.observe(&ev(line));
        }
        let doc = partial_record(&p, StartupPhase::Ttl, "/t.jsonl", &id());
        assert_eq!(doc["stage"], "ttl");
        assert_eq!(doc["bootMs"], 1125);
        assert_eq!(doc["ttlMs"], 297_016);
        assert_eq!(doc["startupToolCalls"], 1);
        assert_eq!(doc["firstMutationIndex"], 1);
        for absent in ["startupMs", "toolCalls", "startupPct", "durationMs"] {
            assert!(
                doc.get(absent).is_none(),
                "a ttl partial cannot know {absent}"
            );
        }
    }

    /// How a consumer tells old records from new: every record this build writes carries `stage`,
    /// so `stage` ABSENT means the record predates the split and has no boot/ttl to read.
    #[test]
    fn every_emitted_record_carries_a_stage() {
        let m = RunMetrics {
            boot_ms: Some(1125),
            ttl_ms: Some(297_016),
            startup_ms: Some(166_559),
            ..RunMetrics::default()
        };
        let doc = final_record(
            "/t.jsonl",
            &m,
            &id(),
            Some((0, TraceOutcome::Ok)),
            &ToolingReport::default(),
            &[],
        );
        assert_eq!(doc["stage"], STAGE_FINAL);
        assert_eq!(doc["bootMs"], 1125);
        assert_eq!(doc["ttlMs"], 297_016);
        assert_eq!(doc["startupMs"], 166_559);
        assert_eq!(doc["outcome"], "ok");
        let mut p = StartupProbe::default();
        p.observe(&ev(&text_at("2026-07-28T05:36:13.214Z")));
        p.observe(&ev(&tool_at(
            "2026-07-28T05:36:14.339Z",
            "mcp__fsm__unvetted",
        )));
        assert_eq!(
            partial_record(&p, StartupPhase::Boot, "/t.jsonl", &id())["stage"],
            "boot"
        );
    }

    /// A bare `run-metrics <trace>` (no identity flags) must stay re-derivable from an archived
    /// trace: the dashboard rebuilds history that way, so nothing here may need runner state.
    #[test]
    fn a_bare_record_omits_identity_but_keeps_the_split() {
        let m = run_metrics(&vetter_20260728t053610z());
        let bare = RunIdentity {
            run_id: None,
            role: None,
            model: None,
        };
        let doc = final_record("/t.jsonl", &m, &bare, None, &ToolingReport::default(), &[]);
        assert!(doc.get("runId").is_none());
        assert!(doc.get("role").is_none());
        assert!(doc.get("outcome").is_none());
        assert_eq!(doc["bootMs"], 1125);
        assert_eq!(doc["ttlMs"], 297_016);
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
    fn read_json(rel: &str) -> Option<Value> {
        let path = format!("{}/../{}", env!("CARGO_MANIFEST_DIR"), rel);
        let text = std::fs::read_to_string(&path).ok()?;
        Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}")))
    }

    fn perm_list(rel: &str, which: &str) -> Option<Vec<String>> {
        let v = read_json(rel)?;
        Some(
            v["permissions"][which]
                .as_array()
                .unwrap_or_else(|| panic!("{rel}: permissions.{which} is not an array"))
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        )
    }

    fn deny_list(rel: &str) -> Option<Vec<String>> {
        perm_list(rel, "deny")
    }

    fn read_text(rel: &str) -> Option<String> {
        std::fs::read_to_string(format!("{}/../{}", env!("CARGO_MANIFEST_DIR"), rel)).ok()
    }

    // The wiring above only means something if the RUNNER launches it. `review-run.sh` is the only
    // thing that starts the vetter, so this pins the last side: it names the prompt/settings that
    // exist, passes the MCP server config with `--strict-mcp-config`, and offers NO second surface —
    // one assignment each, and no environment flag anywhere that could select a different one. Two
    // vetter configurations where one is unreachable is drift a reader cannot resolve.
    #[test]
    fn the_vetter_runner_launches_the_mcp_surface_and_only_that() {
        let Some(sh) = read_text("review-run.sh") else {
            return; // not checked out (nix build sandbox) — enforced by the rs-test gate
        };

        for needed in [
            "PROMPT_FILE=\"$DIR/review-prompt.txt\"",
            "SETTINGS_FILE=\"$DIR/review-settings.json\"",
            "MCP_ARGS=(--mcp-config \"$DIR/review-mcp.json\" --strict-mcp-config)",
            "--settings \"$SETTINGS_FILE\"",
            "\"${MCP_ARGS[@]}\"",
        ] {
            assert!(
                sh.contains(needed),
                "review-run.sh must launch the vetter with {needed}"
            );
        }

        // A second assignment is a second surface — i.e. a branch the reader has to resolve.
        for once in ["PROMPT_FILE=", "SETTINGS_FILE=", "MCP_ARGS="] {
            assert_eq!(
                sh.matches(once).count(),
                1,
                "review-run.sh must assign {once} exactly once: the vetter has one tool surface"
            );
        }

        // The files it names must be the ones on disk, or the cron sed's an empty prompt / passes a
        // missing settings path and the whole surface silently evaporates.
        for f in [
            "review-prompt.txt",
            "review-settings.json",
            "review-mcp.json",
        ] {
            assert!(
                read_text(f).is_some(),
                "review-run.sh names {f}, which must exist"
            );
        }

        // No opt-in flag survives in the runner or the deployment-config template.
        for f in ["review-run.sh", "cron.env.example"] {
            let Some(text) = read_text(f) else { continue };
            assert!(
                !text.contains("VETTER_MCP"),
                "{f}: the vetter's surface is not selectable — no VETTER_MCP"
            );
        }
    }

    // The MCP-mode settings only mean anything if every allowed tool name is one the server actually
    // exposes: Claude Code presents an MCP tool as `mcp__<server>__<tool>`, and a subtly wrong name
    // fails at run time looking exactly like the tool not existing. This pins the three sides
    // together — the server name in `review-mcp.json`, the tool names the binary emits from
    // `tools/list`, and the allow-list the cron would run with.
    #[test]
    fn mcp_wiring_names_match_the_server() {
        for (cfg_file, settings_file, args, profile) in [
            (
                "review-mcp.json",
                "review-settings.json",
                serde_json::json!(["mcp"]),
                super::McpProfile::Vetter,
            ),
            (
                "campaign-mcp.json",
                "campaign-settings.json",
                serde_json::json!(["mcp", "--profile", "producer"]),
                super::McpProfile::Producer,
            ),
        ] {
            let Some(cfg) = read_json(cfg_file) else {
                return; // not checked out (nix build sandbox) — enforced by the rs-test gate
            };
            let servers = cfg["mcpServers"].as_object().expect("mcpServers object");
            assert_eq!(servers.len(), 1, "{cfg_file}: one server: the FSM");
            let (name, spec) = servers.iter().next().unwrap();
            assert_eq!(
                name,
                super::MCP_SERVER_NAME,
                "{cfg_file}: server key must be the name the binary reports"
            );
            assert_eq!(spec["command"], serde_json::json!("pr-review-report"));
            assert_eq!(spec["args"], args, "{cfg_file}: profile flag must match");

            let expected: Vec<String> = super::mcp_tools(profile)
                .as_array()
                .unwrap()
                .iter()
                .map(|t| format!("mcp__{name}__{}", t["name"].as_str().unwrap()))
                .collect();
            let allow = perm_list(settings_file, "allow").expect("settings");
            let allowed_mcp: Vec<String> = allow
                .iter()
                .filter(|a| a.starts_with("mcp__"))
                .cloned()
                .collect();
            assert_eq!(
                allowed_mcp, expected,
                "{settings_file}: the allow-list must name exactly the tools the server exposes"
            );
        }
    }

    // The producer keeps Bash (it builds, tests and pushes), so its MCP tools have to be REACHABLE
    // rather than merely present: Claude Code defers MCP schemas behind ToolSearch, and a run that
    // cannot call ToolSearch sees them as nonexistent (the failure #63 hit).
    #[test]
    fn the_producer_can_reach_its_mcp_tools() {
        let Some(allow) = perm_list("campaign-settings.json", "allow") else {
            return;
        };
        assert!(
            allow.iter().any(|a| a == "ToolSearch"),
            "MCP tool schemas are deferred; without ToolSearch the clone tools are unreachable"
        );
    }

    // #78: the vetter's surface is thirteen tools, fixed and tiny. Deferring it buys nothing and
    // costs a `ToolSearch` round trip every run to rediscover a hardcoded allowlist, so the runner
    // puts the harness in standard mode instead.
    //
    // The second half is the part that matters: `ToolSearch` stays ALLOWED. The export is an
    // optimisation and must degrade to the old behaviour, never to a dead vetter — a run that
    // cannot call `ToolSearch` while schemas are deferred sees its own tools as nonexistent and
    // records nothing (#63). Allowed-and-unused costs one schema; disallowed-and-needed costs a run.
    #[test]
    fn the_vetter_presents_its_surface_instead_of_deferring_it() {
        let Ok(runner) = std::fs::read_to_string("review-run.sh") else {
            return;
        };
        assert!(
            runner.contains("export ENABLE_TOOL_SEARCH=false"),
            "review-run.sh must put the harness in standard mode so the tiny FSM surface is \
             presented rather than deferred behind a ToolSearch round trip"
        );
        let Some(allow) = perm_list("review-settings.json", "allow") else {
            return;
        };
        assert!(
            allow.iter().any(|a| a == "ToolSearch"),
            "ToolSearch must stay allowed as the fail-safe: if the harness defers anyway, a vetter \
             that cannot call it records nothing at all"
        );
    }

    // The whole point of the clone tools: the producer prompt must no longer mandate a `rm -rf` that
    // the deny-list prefix-matches into unusability (#56).
    #[test]
    fn the_producer_prompt_releases_clones_through_the_tool_not_rm_rf() {
        let Ok(prompt) = std::fs::read_to_string("campaign-prompt.txt") else {
            return;
        };
        assert!(
            !prompt.contains("rm -rf <clonedir>"),
            "campaign-prompt must not mandate `rm -rf <clonedir>` — a prefix-matched deny rule makes \
             it impossible to follow, which is how 195 GB accumulated (#56)"
        );
        assert!(
            prompt.contains("never `rm -rf` a clone"),
            "the prompt must say so explicitly, not merely omit the old instruction"
        );
        // Every clone-lifecycle move the producer makes is named as a tool.
        for tool in [
            "mcp__fsm__clone_create",
            "mcp__fsm__clone_release",
            "mcp__fsm__clone_gc",
        ] {
            assert!(prompt.contains(tool), "the prompt must name {tool}");
        }
        // …and the old shell recipes for those moves are gone.
        assert!(!prompt.contains("pr-review-report gc-clones"));
        assert!(!prompt.contains("git -C <dir> fetch origin &&"));
    }

    // MCP mode's whole claim is that a non-FSM operation is UNREPRESENTABLE: no Bash at all, so no
    // raw `gh`/`git`, and no prefix-matched deny-list to route around.
    #[test]
    fn mcp_vetter_has_no_bash() {
        let Some(deny) = deny_list("review-settings.json") else {
            return;
        };
        assert!(
            deny.iter().any(|d| d == "Bash"),
            "MCP mode must deny Bash outright"
        );
        let allow = perm_list("review-settings.json", "allow").unwrap();
        assert!(
            !allow.iter().any(|a| a == "Bash" || a.starts_with("Bash(")),
            "MCP mode must not allow any Bash form"
        );
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
        checkout_dir, gc_decision, is_vet_checkout, nix_gc_args, parse_pr_state, parse_repo_slug,
        should_nix_gc, CloneState, GcAction, PrState, VET_CLONE_MAX_AGE_DAYS,
    };

    fn st(clean: bool, unpushed: Option<u32>, pr: Option<PrState>, age_days: u64) -> CloneState {
        CloneState {
            clean,
            unpushed,
            pr,
            age_days,
            vet: false,
        }
    }

    /// The same clone, but an audit-lens checkout (`vet-<repo>-<n>`).
    fn vst(clean: bool, unpushed: Option<u32>, pr: Option<PrState>, age_days: u64) -> CloneState {
        CloneState {
            vet: true,
            ..st(clean, unpushed, pr, age_days)
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

    // #81: the leak. A `vet-*` checkout is a copy of the PR the vetter is JUDGING, so its PR is
    // always OPEN — and `gc_keeps_open_pr` therefore made every leaked audit checkout immortal. 83
    // of them, 349 MB, under a sweep that had been running nightly the whole time. The vet arm must
    // ignore PR state entirely.
    #[test]
    fn gc_reclaims_a_stale_vet_checkout_whose_pr_is_still_open() {
        assert_eq!(
            gc_decision(&vst(true, Some(0), Some(PrState::Open), 17), 30),
            GcAction::Delete("vet checkout, idle 17d".into()),
            "an open PR must not keep an audit-lens checkout alive"
        );
        // Its OWN cap, not the caller's: at `--max-age-days 365` a leaked checkout is still reaped.
        assert_eq!(
            gc_decision(&vst(true, Some(0), Some(PrState::Open), 2), 365),
            GcAction::Delete("vet checkout, idle 2d".into())
        );
        // A merged/closed PR reads the same way — one rule, not two.
        assert_eq!(
            gc_decision(&vst(true, Some(0), Some(PrState::Merged), 5), 30),
            GcAction::Delete("vet checkout, idle 5d".into())
        );
        assert_eq!(
            gc_decision(&vst(true, Some(0), None, 5), 30),
            GcAction::Delete("vet checkout, idle 5d".into())
        );
    }

    // The other side of the cap: a checkout the RUNNING vetter is reading is not residue. The
    // vetter's own `REVIEW_MAXTIME` ceiling is 2h, so a same-day checkout can still be in use.
    #[test]
    fn gc_keeps_a_vet_checkout_younger_than_its_cap() {
        assert_eq!(
            gc_decision(&vst(true, Some(0), Some(PrState::Open), 0), 30),
            GcAction::Keep("vet checkout, idle 0d < 1d".into())
        );
        // Boundary is inclusive, exactly as the no-PR backstop's is.
        assert!(matches!(
            gc_decision(
                &vst(true, Some(0), Some(PrState::Open), VET_CLONE_MAX_AGE_DAYS),
                30
            ),
            GcAction::Delete(_)
        ));
    }

    // The safety guards still win. "Reclaim leaked checkouts" must never become "delete work that
    // happens to sit under a vet-* name" — the vet arm is placed AFTER the dirt/unpushed ladder.
    #[test]
    fn gc_never_deletes_a_dirty_or_unpushed_vet_checkout() {
        assert_eq!(
            gc_decision(&vst(false, Some(0), Some(PrState::Open), 99), 30),
            GcAction::Keep("uncommitted changes".into())
        );
        assert_eq!(
            gc_decision(&vst(true, Some(2), Some(PrState::Open), 99), 30),
            GcAction::Keep("2 unpushed commit(s)".into())
        );
        assert_eq!(
            gc_decision(&vst(true, None, Some(PrState::Open), 99), 30),
            GcAction::Keep("unpushed state unknown".into())
        );
    }

    // The classifier and the path builder must agree, or the sweep applies the producer rule to a
    // checkout (the leak) or the vet rule to real work (data loss).
    #[test]
    fn vet_classifier_matches_what_pr_checkout_actually_creates() {
        let dir = checkout_dir("/work", "rainlanguage/rain.factory", 47);
        let name = dir.rsplit('/').next().unwrap();
        assert_eq!(name, "vet-rain.factory-47");
        assert!(is_vet_checkout(name));
        // Producer work clones are named after the ISSUE, never `vet-`.
        assert!(!is_vet_checkout("raindex-2444"));
        assert!(!is_vet_checkout("cyclo.site"));
        // The improvised names the vetter left behind before `pr_checkout` returned its dir are
        // still checkouts, and still reclaimable.
        assert!(is_vet_checkout("vet-st0x.deploy-243-h2"));
        assert!(is_vet_checkout("vet-rain.factory-dep"));
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
                work_dirs: s(&["/w"]),
                dry_run: false,
                max_age_days: 30,
            }
        );
        assert_eq!(
            parse(&["prr", "gc-clones", "/w", "--dry-run", "--max-age-days", "7"]),
            Cmd::GcClones {
                work_dirs: s(&["/w"]),
                dry_run: true,
                max_age_days: 7,
            }
        );
        // SEVERAL roots in one sweep: the vetter's stranded `vet-*` clones live in the install dir,
        // not WORK_DIR, so a one-root sweep never reclaimed them.
        assert_eq!(
            parse(&["prr", "gc-clones", "/w", "/install", "--dry-run"]),
            Cmd::GcClones {
                work_dirs: s(&["/w", "/install"]),
                dry_run: true,
                max_age_days: 30,
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
                work_dirs: s(&["/w"]),
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
                work_dirs: vec![],
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
                work_dirs: s(&["/w"]),
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
                work_dirs: s(&["/w"]),
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
        // Bare form is unchanged: the enrichment flags are all optional, so the dashboard's
        // re-derivation from raw traces keeps working untouched.
        assert_eq!(
            parse(&["prr", "run-metrics", "/path/to/trace.jsonl"]),
            Cmd::RunMetrics {
                trace: "/path/to/trace.jsonl".to_string(),
                run_id: None,
                role: None,
                model: None,
                exit_code: None,
                preflight_missing: vec![],
            }
        );
        // The form the runners now use in place of the `| jq '. + {…}'` pipe.
        assert_eq!(
            parse(&[
                "prr",
                "run-metrics",
                "/t.jsonl",
                "--run-id",
                "20260727T100743Z",
                "--role",
                "producer",
                "--model",
                "claude-fable-5",
                "--exit-code",
                "0"
            ]),
            Cmd::RunMetrics {
                trace: "/t.jsonl".to_string(),
                run_id: Some("20260727T100743Z".to_string()),
                role: Some("producer".to_string()),
                model: Some("claude-fable-5".to_string()),
                exit_code: Some(0),
                preflight_missing: vec![],
            }
        );
        // The abort form: `preflight` found nothing to render with, so the model never started.
        assert_eq!(
            parse(&[
                "prr",
                "run-metrics",
                "/t.jsonl",
                "--exit-code",
                "12",
                "--preflight-missing",
                "pdftoppm,pdfinfo"
            ]),
            Cmd::RunMetrics {
                trace: "/t.jsonl".to_string(),
                run_id: None,
                role: None,
                model: None,
                exit_code: Some(12),
                preflight_missing: vec!["pdftoppm".to_string(), "pdfinfo".to_string()],
            }
        );
    }

    #[test]
    fn run_timings_cli() {
        assert_eq!(
            parse(&[
                "prr",
                "run-timings",
                "--out",
                "/m/runs.jsonl",
                "--trace",
                "/r/20260728T053610Z.jsonl",
                "--run-id",
                "20260728T053610Z",
                "--role",
                "vetter",
                "--model",
                "claude-fable-5",
            ]),
            Cmd::RunTimings {
                out: "/m/runs.jsonl".to_string(),
                trace: "/r/20260728T053610Z.jsonl".to_string(),
                run_id: Some("20260728T053610Z".to_string()),
                role: Some("vetter".to_string()),
                model: Some("claude-fable-5".to_string()),
            }
        );
    }

    #[test]
    fn closure_gate_cli() {
        // Three gates, three subcommands, one flake flag each — so `rust.yml` is an invocation and
        // an exit code rather than 135 lines of bash nothing can test.
        assert_eq!(
            parse(&["prr", "closure-preflight"]),
            Cmd::ClosurePreflight {
                flake: ".".to_string()
            },
            "CI runs in the checkout, so `.` is the default"
        );
        assert_eq!(
            parse(&["prr", "closure-render"]),
            Cmd::ClosureRender {
                flake: ".".to_string()
            }
        );
        assert_eq!(
            parse(&["prr", "closure-surface"]),
            Cmd::ClosureSurface {
                flake: ".".to_string()
            }
        );
        assert_eq!(
            parse(&["prr", "closure-surface", "--flake", "/srv/issue-pr-cron"]),
            Cmd::ClosureSurface {
                flake: "/srv/issue-pr-cron".to_string()
            }
        );
    }

    #[test]
    fn trace_outcome_cli() {
        assert_eq!(
            parse(&["prr", "trace-outcome", "/t.jsonl"]),
            Cmd::TraceOutcome {
                trace: "/t.jsonl".to_string(),
                exit_code: 0,
            }
        );
        assert_eq!(
            parse(&["prr", "trace-outcome", "/t.jsonl", "--exit-code", "124"]),
            Cmd::TraceOutcome {
                trace: "/t.jsonl".to_string(),
                exit_code: 124,
            }
        );
    }

    #[test]
    fn queue_history_line_cli() {
        // Path form (refresh-human-queue.sh).
        assert_eq!(
            parse(&[
                "prr",
                "queue-history-line",
                "/q.json",
                "--ts",
                "2026-07-27T10:00:00Z"
            ]),
            Cmd::QueueHistoryLine {
                snapshot: Some("/q.json".to_string()),
                ts: "2026-07-27T10:00:00Z".to_string(),
            }
        );
        // Stdin form (backfill-human-queue-history.sh pipes `git show` into it).
        assert_eq!(
            parse(&["prr", "queue-history-line", "--ts", "2026-07-27T10:00:00Z"]),
            Cmd::QueueHistoryLine {
                snapshot: None,
                ts: "2026-07-27T10:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn distill_trace_cli() {
        assert_eq!(parse(&["prr", "distill-trace"]), Cmd::DistillTrace);
    }

    // ---- trace classification: the typed replacement for the runners' fallback grep ----

    #[test]
    fn quota_limit_from_typed_status_field() {
        let t = r#"{"type":"result","subtype":"error","api_error_status":429}"#;
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
        // The SDK has also written it as a string.
        let t = r#"{"type":"result","subtype":"error","api_error_status":"429"}"#;
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
    }

    #[test]
    fn quota_limit_from_subtype_and_from_result_text() {
        let t = r#"{"type":"result","subtype":"error_usage_limit"}"#;
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
        let t =
            r#"{"type":"result","subtype":"error","result":"You have reached your usage limit."}"#;
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
    }

    // The whole point of moving this out of `grep`. The old regex scanned the raw trace, so a 429
    // (or the words "usage limit") appearing inside a tool RESULT — a fetched page, a quoted CI
    // log, a PR body the run happened to read — advanced model fallback for a run that was never
    // quota-limited. Here only `result` events are consulted, so this trace is a clean success.
    #[test]
    fn quota_words_inside_tool_output_do_not_trip_fallback() {
        let t = concat!(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"HTTP 429: you have reached your usage limit"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","result":"opened 3 PRs"}"#,
        );
        assert_eq!(classify_trace(t, 0), TraceOutcome::Ok);
        // …and the same bytes in an assistant message are equally inert.
        let t = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the API returned api_error_status 429 earlier"}]}}"#;
        assert_eq!(classify_trace(t, 0), TraceOutcome::Ok);
    }

    #[test]
    fn nonzero_exit_without_quota_signal_is_error() {
        let t = r#"{"type":"result","subtype":"success","result":"done"}"#;
        assert_eq!(classify_trace(t, 124), TraceOutcome::Error); // timeout(1)
                                                                 // An empty trace (claude died before emitting anything) follows the exit code.
        assert_eq!(classify_trace("", 1), TraceOutcome::Error);
        assert_eq!(classify_trace("", 0), TraceOutcome::Ok);
    }

    #[test]
    fn quota_wins_over_exit_code_so_fallback_still_advances() {
        // A quota-limited run also exits non-zero; it must classify as the limit, not as a
        // generic error, or the loop would stop instead of trying the next model.
        let t = r#"{"type":"result","subtype":"error","api_error_status":429}"#;
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
    }

    #[test]
    fn malformed_trace_lines_are_skipped_not_fatal() {
        let t = concat!(
            "not json at all\n",
            "\n",
            r#"{"type":"result","subtype":"error","api_error_status":429}"#,
            "\n{\"truncated\": ",
        );
        assert_eq!(classify_trace(t, 1), TraceOutcome::QuotaLimited);
    }

    #[test]
    fn outcome_words_match_the_committed_history() {
        // metrics/runs.jsonl already contains these three words and the dashboard reads them.
        assert_eq!(TraceOutcome::Ok.as_str(), "ok");
        assert_eq!(TraceOutcome::QuotaLimited.as_str(), "session-limit");
        assert_eq!(TraceOutcome::Error.as_str(), "error");
        assert_eq!(TraceOutcome::ToolingFailure.as_str(), "tooling-failure");
    }

    // ---- tooling failures (#85) ----------------------------------------------------------
    //
    // The fixtures below are cut down from the real trace that produced #85
    // (review-runs/20260728T055940Z.jsonl): a Glob that enumerated the repo, then two Reads of
    // audit PDFs that came back `is_error` because the closure had no `pdftoppm`.

    /// One assistant `tool_use` event.
    fn tu(id: &str, name: &str, input: Value) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}
        })
        .to_string()
    }

    /// One `tool_result` event for a given tool_use id.
    fn tr(id: &str, content: &str, is_error: bool) -> String {
        serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}
            ]}
        })
        .to_string()
    }

    const PDF_A: &str = "/home/gildlab/code/vet-x/audit/protofire/a.pdf";
    const PDF_B: &str = "/home/gildlab/code/vet-x/audit/protofire/b.pdf";
    // The harness's own words. Nothing in the classifier reads them; they are here so a future
    // reader can see the test does not depend on them.
    const POPPLER_ERR: &str = "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.";

    fn blind_pdf_trace() -> String {
        [
            tu("g1", "Glob", serde_json::json!({"pattern": "**/*"})),
            tr(
                "g1",
                &format!("{PDF_A}\n{PDF_B}\n/home/gildlab/code/vet-x/README.md"),
                false,
            ),
            tu("r1", "Read", serde_json::json!({"file_path": PDF_A})),
            tr("r1", POPPLER_ERR, true),
            tu("r2", "Read", serde_json::json!({"file_path": PDF_B})),
            tr("r2", POPPLER_ERR, true),
            r#"{"type":"result","subtype":"success","num_turns":9}"#.to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn a_read_that_fails_on_an_enumerated_file_is_a_tooling_failure() {
        let r = trace_tooling_report(&blind_pdf_trace());
        assert_eq!(r.unreadable, vec![PDF_A.to_string(), PDF_B.to_string()]);
        // And it decides the run's outcome, on a trace claude exited 0 on.
        assert_eq!(
            classify_trace(&blind_pdf_trace(), 0),
            TraceOutcome::ToolingFailure,
            "a run that could not read evidence the filesystem listed is not `ok`"
        );
    }

    #[test]
    fn the_classifier_never_reads_the_error_message() {
        // Same structure, a message with no hint of a missing tool in it. If the rule were
        // matching prose ("is not installed", "poppler"), this would come back `ok` — which is
        // exactly how the NEXT undeclared dependency would slip through.
        let t = blind_pdf_trace().replace(POPPLER_ERR, "Could not render this file.");
        assert!(!t.contains("pdftoppm"));
        assert_eq!(classify_trace(&t, 0), TraceOutcome::ToolingFailure);
    }

    #[test]
    fn a_read_of_a_path_nothing_listed_is_an_input_error_not_a_tooling_failure() {
        // The model guessed at a filename. The environment never said it existed, so this is the
        // run doing its job — and making it red would spend the outcome's credibility.
        let t = [
            tu(
                "r1",
                "Read",
                serde_json::json!({"file_path": "/home/gildlab/code/vet-x/nope.pdf"}),
            ),
            tr("r1", "File does not exist.", true),
        ]
        .join("\n");
        assert_eq!(trace_tooling_report(&t), ToolingReport::default());
        assert_eq!(classify_trace(&t, 0), TraceOutcome::Ok);
    }

    #[test]
    fn only_a_successful_glob_corroborates() {
        // A FAILED Glob's output is not an enumeration of anything.
        let t = [
            tu("g1", "Glob", serde_json::json!({"pattern": "**/*"})),
            tr("g1", &format!("Ripgrep search timed out\n{PDF_A}"), true),
            tu("r1", "Read", serde_json::json!({"file_path": PDF_A})),
            tr("r1", POPPLER_ERR, true),
        ]
        .join("\n");
        assert!(trace_tooling_report(&t).unreadable.is_empty());
    }

    #[test]
    fn grep_hits_do_not_corroborate() {
        // Grep lines are `path:line:text`, so the path is a prefix of the line rather than the
        // line. Counting them would mean parsing output shape instead of reading an enumeration.
        let t = [
            tu("g1", "Grep", serde_json::json!({"pattern": "pdf"})),
            tr("g1", &format!("{PDF_A}:5:  \"pdf\": \"a.pdf\","), false),
            tu("r1", "Read", serde_json::json!({"file_path": PDF_A})),
            tr("r1", POPPLER_ERR, true),
        ]
        .join("\n");
        assert!(trace_tooling_report(&t).unreadable.is_empty());
    }

    #[test]
    fn corroboration_is_order_independent() {
        // The Glob lands AFTER the failed Read. Still corroborated: the question is whether the
        // file existed, not when the run learned it did.
        let t = [
            tu("r1", "Read", serde_json::json!({"file_path": PDF_A})),
            tr("r1", POPPLER_ERR, true),
            tu("g1", "Glob", serde_json::json!({"pattern": "**/*"})),
            tr("g1", PDF_A, false),
        ]
        .join("\n");
        assert_eq!(trace_tooling_report(&t).unreadable, vec![PDF_A.to_string()]);
    }

    #[test]
    fn a_successful_read_of_an_enumerated_file_is_not_a_failure() {
        let t = [
            tu("g1", "Glob", serde_json::json!({"pattern": "**/*"})),
            tr("g1", PDF_A, false),
            tu("r1", "Read", serde_json::json!({"file_path": PDF_A})),
            tr("r1", "<pdf pages>", false),
        ]
        .join("\n");
        assert_eq!(classify_trace(&t, 0), TraceOutcome::Ok);
    }

    #[test]
    fn quota_outranks_a_tooling_failure() {
        // The fallback loop keys on `session-limit` to advance models. A run the API refused never
        // got far enough for its tools to be the problem, so quota must still win.
        let t = format!(
            "{}\n{}",
            blind_pdf_trace(),
            r#"{"type":"result","subtype":"error","api_error_status":429}"#
        );
        assert_eq!(classify_trace(&t, 1), TraceOutcome::QuotaLimited);
    }

    #[test]
    fn bash_exit_127_is_reported_but_does_not_decide_the_outcome() {
        let t = [
            tu(
                "b1",
                "Bash",
                serde_json::json!({"command": "node --version"}),
            ),
            tr(
                "b1",
                "Exit code 127\n/bin/bash: line 1: node: command not found",
                true,
            ),
        ]
        .join("\n");
        let r = trace_tooling_report(&t);
        assert_eq!(r.command_not_found, vec!["node --version".to_string()]);
        assert!(r.unreadable.is_empty());
        // Surfaced, not raised: `which node npm` is a legitimate probe for a tool the run does not
        // need, and 2 of this box's 5 historical 127s are exactly that.
        assert_eq!(classify_trace(&t, 0), TraceOutcome::Ok);
    }

    #[test]
    fn a_bash_failure_that_is_not_127_is_not_a_missing_command() {
        for first in [
            "Exit code 1",
            "Exit code 12",
            "Exit code 1270",
            "Permission denied",
        ] {
            let t = [
                tu("b1", "Bash", serde_json::json!({"command": "gh pr list"})),
                tr("b1", &format!("{first}\nsomething went wrong"), true),
            ]
            .join("\n");
            assert!(
                trace_tooling_report(&t).command_not_found.is_empty(),
                "{first:?} must not read as command-not-found"
            );
        }
    }

    #[test]
    fn preflight_failure_outranks_everything_and_needs_no_trace() {
        // The model never ran, so there is no trace to classify. The outcome comes from the fact
        // that a declared binary would not resolve.
        let missing = vec!["pdftoppm".to_string()];
        assert_eq!(
            classify_outcome("", 12, &missing),
            TraceOutcome::ToolingFailure
        );
        // …and an empty list must not change what the trace already said.
        assert_eq!(classify_outcome("", 0, &[]), TraceOutcome::Ok);
    }

    #[test]
    fn the_harness_read_dependencies_stay_declared() {
        // These are named NOWHERE else in the repo: no script, no prompt, no skill mentions them.
        // This list and the closure check in .github/workflows/rust.yml are the only two places a
        // read-time dependency exists at all, so dropping one here must fail a named test.
        let bins: Vec<&str> = HARNESS_TOOLS.iter().map(|t| t.bin).collect();
        assert!(bins.contains(&"pdftoppm"), "PDF rendering: {bins:?}");
        assert!(bins.contains(&"pdfinfo"), "PDF page count: {bins:?}");
        for t in HARNESS_TOOLS {
            assert!(
                !t.why.trim().is_empty(),
                "{} must carry the evidence for needing it",
                t.bin
            );
        }
    }

    #[test]
    fn resolve_in_finds_only_executables_in_named_directories() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("prr-preflight-{}", std::process::id()));
        let bin = root.join("bin");
        let other = root.join("other");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(bin.join("real-tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.join("real-tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // Present but not executable — a data file of the same name is not the tool.
        std::fs::write(bin.join("not-exec"), "x").unwrap();
        // Present, executable, but in a directory PATH does not name.
        std::fs::write(other.join("unlisted"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            other.join("unlisted"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let path = std::ffi::OsString::from(bin.to_string_lossy().to_string());
        assert_eq!(
            resolve_in(&path, "real-tool"),
            Some(bin.join("real-tool")),
            "an executable on PATH resolves"
        );
        assert_eq!(resolve_in(&path, "not-exec"), None, "not executable");
        assert_eq!(resolve_in(&path, "unlisted"), None, "not on PATH");
        assert_eq!(resolve_in(&path, "absent"), None);
        // An empty PATH element must not mean "look in the cwd".
        let with_empty = std::ffi::OsString::from(format!(":{}", bin.to_string_lossy()));
        assert_eq!(
            resolve_in(&with_empty, "real-tool"),
            Some(bin.join("real-tool"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- queue-history-line: one implementation for the live append and the backfill ----

    // Byte-identical to what the jq this replaces emitted, key order included — `ts` first, and
    // the counts sub-object in the snapshot's own order. That parity is why serde_json carries the
    // `preserve_order` feature; see the note in Cargo.toml.
    #[test]
    fn queue_history_line_shape() {
        let snap = r#"{"counts":{"ready":3,"design":1},"other":"ignored"}"#;
        assert_eq!(
            queue_history_line(snap, "2026-07-27T10:00:00Z").unwrap(),
            r#"{"ts":"2026-07-27T10:00:00Z","counts":{"ready":3,"design":1}}"#
        );
        // Only `counts` is carried over; anything else in the snapshot is dropped.
        let parsed: Value = serde_json::from_str(&queue_history_line(snap, "t").unwrap()).unwrap();
        assert_eq!(
            parsed.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["ts", "counts"]
        );
    }

    #[test]
    fn queue_history_line_skips_snapshots_without_counts() {
        // The backfill's `select(.counts != null)`: early commits of human-queue.json predate the
        // key, and those commits must contribute zero bytes rather than a `null` line.
        assert!(queue_history_line(r#"{"prs":[]}"#, "t").is_none());
        assert!(queue_history_line(r#"{"counts":null}"#, "t").is_none());
        // `git show` of a commit where the file did not exist yields empty output.
        assert!(queue_history_line("", "t").is_none());
        assert!(queue_history_line("not json", "t").is_none());
    }

    // ---- distill-trace: the jq distiller, with its truncation widths now under test ----

    #[test]
    fn distill_tool_use_prefers_command_then_description_then_input() {
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Bash","input":{"command":"gh pr list","description":"ignored"}}]}});
        assert_eq!(distill_event(&ev), vec!["  ▸ Bash  gh pr list"]);

        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"description":"read a file"}}]}});
        assert_eq!(distill_event(&ev), vec!["  ▸ Read  read a file"]);

        // Neither key: the whole input object, as jq's `(.input|tostring)` did.
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"X","input":{"a":1}}]}});
        assert_eq!(distill_event(&ev), vec![r#"  ▸ X  {"a":1}"#]);
    }

    #[test]
    fn distill_flattens_newlines_and_clips_at_the_jq_widths() {
        let long = "x".repeat(500);
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":long}]}});
        let out = &distill_event(&ev)[0];
        assert_eq!(
            out.chars().count(),
            4 + 200,
            "text clips to 200 after '  · '"
        );

        let long = "y".repeat(1000);
        let ev = serde_json::json!({"type":"result","subtype":"success","result":long});
        let out = &distill_event(&ev)[0];
        assert!(out.ends_with(&"y".repeat(800)));
        assert_eq!(out.chars().count(), "  ⟹ SUCCESS: ".chars().count() + 800);

        // Newlines become spaces so one event stays one log line.
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"a\nb\nc"}]}});
        assert_eq!(distill_event(&ev), vec!["  · a b c"]);
    }

    // jq sliced CODEPOINTS (`.[0:200]`). Clipping bytes instead would split a multi-byte glyph
    // and write invalid UTF-8 into the log the humans read.
    #[test]
    fn distill_clips_multibyte_text_by_character() {
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"é".repeat(300)}]}});
        let out = &distill_event(&ev)[0];
        assert_eq!(out.chars().filter(|c| *c == 'é').count(), 200);
    }

    #[test]
    fn distill_result_defaults_subtype_and_uppercases_it() {
        let ev = serde_json::json!({"type":"result","result":"done"});
        assert_eq!(distill_event(&ev), vec!["  ⟹ DONE: done"]);
        let ev = serde_json::json!({"type":"result","subtype":"error_max_turns","result":""});
        assert_eq!(distill_event(&ev), vec!["  ⟹ ERROR_MAX_TURNS: "]);
    }

    #[test]
    fn distill_emits_one_line_per_content_item_and_ignores_other_events() {
        let ev = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"thinking"},
            {"type":"tool_use","name":"Bash","input":{"command":"ls"}},
            {"type":"thinking","thinking":"hidden"}]}});
        assert_eq!(distill_event(&ev), vec!["  · thinking", "  ▸ Bash  ls"]);

        // `user`/`system` events produced nothing under the jq filter either.
        assert!(distill_event(&serde_json::json!({"type":"user"})).is_empty());
        assert!(distill_event(&serde_json::json!({"type":"system","subtype":"init"})).is_empty());
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
            has_human_override: false,
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
    fn human_override_parks_over_stale_ai_label_ci_and_deploy() {
        // Control: a red PR carrying a stale `ai:ready` (no human override) routes to Needs3b — the
        // pre-fix behaviour that mis-routed human-decided PRs as routine work.
        let mut s = sig(Ci::Red, "DIRTY");
        s.state_label = Some("ai:ready".to_string());
        assert_eq!(
            next_action(&s),
            NextAction::Needs3b,
            "control: no human override → a red PR routes to 3b"
        );
        // A human decision BLOCKS routine action: the PR is parked regardless of the stale
        // `ai:ready`, the red CI, AND a deploy trigger (otherwise checked before CI).
        s.has_human_override = true;
        s.has_deploy_trigger = true;
        assert_eq!(next_action(&s), NextAction::ParkedSkip);
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
    fn worklist_row_deploy_done_accepts_12_char_short_sha() {
        // A deploy-confirmed note embedding a 12-char SHORT sha marks a 40-char head done — the
        // >=12-char-prefix branch. The other head-scoped tests use <12-char heads (head_short == head),
        // so this is the only case that exercises the prefix match.
        let head = "abcdef0123456789abcdef0123456789abcdef01"; // 40 chars; 12-char prefix = abcdef012345
        let done = json!({
            "number": 7, "url": "", "title": "t", "headRefOid": head,
            "body": "REQUIRES redeploy at land",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [],
            "comments": [{"author":{"login":"thedavidmeister"},
                          "body":"🤖 ai:producer deploy-confirmed at abcdef012345"}]
        });
        assert_eq!(worklist_row("o/r", &done)["nextAction"], "green-ready");
        // A short sha that does NOT prefix the head is not head-scoped → the redeploy still stands.
        let notdone = json!({
            "number": 7, "url": "", "title": "t", "headRefOid": head,
            "body": "REQUIRES redeploy at land",
            "statusCheckRollup": [{"name":"ci","conclusion":"SUCCESS","status":"COMPLETED"}],
            "mergeStateStatus": "CLEAN", "labels": [],
            "comments": [{"author":{"login":"thedavidmeister"},
                          "body":"🤖 ai:producer deploy-confirmed at 999999999999"}]
        });
        assert_eq!(worklist_row("o/r", &notdone)["nextAction"], "deploy");
    }

    #[test]
    fn producer_backlog_excludes_human_gated_and_close_candidate() {
        let mk = |labels: &[&str]| {
            json!({
                "labels": labels
                    .iter()
                    .map(|n| json!({ "name": n }))
                    .collect::<Vec<_>>()
            })
        };
        // A plain or ai:* uncovered issue IS the producer's backlog.
        assert!(is_producer_backlog(&mk(&[])));
        assert!(is_producer_backlog(&mk(&["bug", "ai:some-label"])));
        // ai:close-candidate → the human's close queue, surfaced separately — not the backlog.
        assert!(!is_producer_backlog(&mk(&["ai:close-candidate"])));
        // Any human:* ruling → the human's inbox, not the producer's.
        assert!(!is_producer_backlog(&mk(&["human:keep-open"])));
        assert!(!is_producer_backlog(&mk(&["human:design"])));
        assert!(!is_producer_backlog(&mk(&["bug", "human:close-candidate"])));
        // Missing labels field → conservatively counted in, never silently dropped.
        assert!(is_producer_backlog(&json!({})));
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
    fn covered_from_native_refs_same_and_cross_repo() {
        // Native references arrive already resolved, whatever textual form produced them
        // (`#N`, `o/r#N`, or the full-URL form issue #54 hit on cyclo.site#406). Coverage is
        // the union across ALL PR nodes, not the first.
        let nodes = vec![
            json!({
                "closingIssuesReferences": {"nodes": [
                    {"number": 318, "repository": {"nameWithOwner": "cyclofinance/cyclo.site"}},
                    {"number": 7, "repository": {"nameWithOwner": "rainlanguage/other"}}
                ]}
            }),
            json!({
                "closingIssuesReferences": {"nodes": [
                    {"number": 12, "repository": {"nameWithOwner": "rainlanguage/second-pr"}}
                ]}
            }),
        ];
        let covered = covered_from_search_prs(&nodes);
        assert!(covered.contains(&("cyclofinance/cyclo.site".to_string(), 318)));
        assert!(covered.contains(&("rainlanguage/other".to_string(), 7)));
        assert!(covered.contains(&("rainlanguage/second-pr".to_string(), 12)));
        assert_eq!(covered.len(), 3);
    }

    #[test]
    fn covered_keyed_by_issue_repo_not_pr_repo() {
        // A cross-repo reference covers the ISSUE's repo; the PR's own repository (present in
        // the node) must not leak into the key.
        let nodes = vec![json!({
            "repository": {"nameWithOwner": "o/pr-repo"},
            "closingIssuesReferences": {"nodes": [
                {"number": 9, "repository": {"nameWithOwner": "o/issue-repo"}}
            ]}
        })];
        let covered = covered_from_search_prs(&nodes);
        assert!(covered.contains(&("o/issue-repo".to_string(), 9)));
        assert!(!covered.contains(&("o/pr-repo".to_string(), 9)));
    }

    #[test]
    fn no_native_refs_means_no_coverage() {
        // Body/title keyword text contributes nothing — only resolved references count (a
        // title-only `Closes #5`, which GitHub never links, is exactly this shape). Empty
        // union members from the search (non-PR nodes) and malformed reference entries
        // (missing number or repository) are tolerated without contributing coverage.
        let nodes = vec![
            json!({"title": "Closes #5", "body": "Fixes #6",
                   "closingIssuesReferences": {"nodes": []}}),
            json!({}),
            json!({"closingIssuesReferences": {"nodes": [
                {"repository": {"nameWithOwner": "o/r"}},
                {"number": 3},
                {}
            ]}}),
        ];
        assert!(covered_from_search_prs(&nodes).is_empty());
    }

    /// One `search` page: `n` nodes tagged with `tag` so the fold's ORDER and COMPLETENESS are
    /// observable, plus the page-info the walk steers on.
    fn search_page(tag: &str, n: usize, next: Option<&str>) -> Value {
        json!({"data": {"search": {
            "pageInfo": {
                "hasNextPage": next.is_some(),
                "endCursor": next.map(Value::from).unwrap_or(Value::Null),
            },
            "nodes": (0..n).map(|i| json!({"tag": format!("{tag}{i}")})).collect::<Vec<_>>(),
        }}})
    }

    fn tags(nodes: &[Value]) -> Vec<String> {
        nodes
            .iter()
            .map(|n| n["tag"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn paged_search_walks_every_page_and_threads_the_cursor() {
        // Three pages, the last one ending the walk. Every page's nodes land, in page order, and
        // each request after the first carries the PREVIOUS page's endCursor.
        let seen = std::cell::RefCell::new(Vec::<Option<String>>::new());
        let got = paged_search_nodes(|cursor| {
            seen.borrow_mut().push(cursor.map(String::from));
            Some(match cursor {
                None => search_page("a", 2, Some("C1")),
                Some("C1") => search_page("b", 1, Some("C2")),
                Some("C2") => search_page("c", 2, None),
                other => panic!("unexpected cursor {other:?}"),
            })
        });
        let (nodes, truncated) = got.expect("a complete walk is Some");
        assert_eq!(tags(&nodes), ["a0", "a1", "b0", "c0", "c1"]);
        assert!(!truncated);
        assert_eq!(
            *seen.borrow(),
            [None, Some("C1".to_string()), Some("C2".to_string())]
        );
    }

    #[test]
    fn paged_search_aborts_the_whole_walk_when_a_page_fails() {
        // THE fail-safe: a mid-walk failure must not degrade to "here are the pages that worked".
        // A short vec is indistinguishable from a complete one, and missing coverage reads as
        // UNCOVERED — the producer would open a duplicate PR for an issue that already has one.
        let calls = std::cell::Cell::new(0);
        let got = paged_search_nodes(|cursor| {
            calls.set(calls.get() + 1);
            match cursor {
                None => Some(search_page("a", 2, Some("C1"))),
                _ => None,
            }
        });
        assert!(got.is_none());
        assert_eq!(calls.get(), 2);

        // A page that comes back without `data.search` (a shape change, a partial GraphQL error)
        // is the same kind of failure, not an empty page.
        assert!(paged_search_nodes(|_| Some(json!({"data": {}}))).is_none());
    }

    #[test]
    fn paged_search_reports_truncation_it_cannot_read_past() {
        // Page cap reached with GitHub still saying `hasNextPage` -> truncated, and exactly
        // `SEARCH_MAX_PAGES` pages were read (not one more, not one fewer) — the cap is the whole
        // 1000-result budget, so stopping early would silently shrink the coverage set.
        let calls = std::cell::Cell::new(0);
        let (nodes, truncated) = paged_search_nodes(|_| {
            calls.set(calls.get() + 1);
            Some(search_page("p", 1, Some("C")))
        })
        .expect("a capped walk still returns what it read");
        assert_eq!(calls.get(), SEARCH_MAX_PAGES);
        assert_eq!(nodes.len(), SEARCH_MAX_PAGES);
        assert!(truncated);

        // `hasNextPage` with no `endCursor` to advance on is the SAME hazard — more pages exist
        // and are unreachable — so it must report truncated too, not pass as a clean finish.
        let (nodes, truncated) = paged_search_nodes(|_| {
            Some(json!({"data": {"search": {
                "pageInfo": {"hasNextPage": true, "endCursor": null},
                "nodes": [{"tag": "x0"}],
            }}}))
        })
        .unwrap();
        assert_eq!(tags(&nodes), ["x0"]);
        assert!(truncated);

        // A single complete page is NOT truncated — otherwise every run would cry wolf.
        let (nodes, truncated) = paged_search_nodes(|_| Some(search_page("s", 1, None))).unwrap();
        assert_eq!(tags(&nodes), ["s0"]);
        assert!(!truncated);
    }

    #[test]
    fn closing_refs_query_pages_githubs_whole_1000_result_cap() {
        // GitHub's `search` connection serves at most 1000 results. The page size in the query and
        // the page cap are two halves of one fact: shrink either and coverage silently stops short
        // of PRs the old `gh search prs --limit 1000` path did see.
        assert_eq!(SEARCH_PAGE_SIZE * SEARCH_MAX_PAGES, 1000);
        assert!(
            CLOSING_REFS_QUERY.contains(&format!("first:{SEARCH_PAGE_SIZE},after:$c")),
            "{CLOSING_REFS_QUERY}"
        );
        // The reference set is keyed off the ISSUE's repo, so the query must ask for it.
        assert!(
            CLOSING_REFS_QUERY.contains("closingIssuesReferences")
                && CLOSING_REFS_QUERY.contains("nodes{number repository{nameWithOwner}}"),
            "{CLOSING_REFS_QUERY}"
        );
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

// ─────────────────────────────────────────────────────────────────────────────
// unvetted (the vetter's state-load) + the MCP FSM surface.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod vetter_state_load_tests {
    use super::*;
    use serde_json::json;

    fn vetter_comment(sha: &str, verdict: &str) -> Value {
        json!({
            "author": {"login": TRUSTED_AUTHOR},
            "body": format!("🤖 ai:vetter\nReviewed {sha}: {verdict} — note\ncost 300 — small diff"),
        })
    }

    // --- vet_action: the vet-lifecycle transition guard -----------------------------------------

    #[test]
    fn un_vetted_pr_is_vetted() {
        assert_eq!(vet_action(false, false, false), VetAction::Vet);
    }

    #[test]
    fn vetted_at_head_is_skipped_and_a_moved_head_re_opens_it() {
        assert_eq!(vet_action(false, false, true), VetAction::SkipVetted);
        // head moved past the last verdict (vetted_at_head false) -> back in the vet queue.
        assert_eq!(vet_action(false, false, false), VetAction::Vet);
    }

    #[test]
    fn drafts_are_left_un_vetted() {
        assert_eq!(vet_action(true, false, false), VetAction::SkipDraft);
    }

    // THE ordering invariant: the human-sacred check resolves BEFORE any head/vetted comparison.
    // rain.erc4626.words#162 (2026-07-04) was re-vetted after a merge-main commit moved the head of a
    // human-REJECTED PR. Here that PR is human-sacred AND head-moved (vetted_at_head=false) — the one
    // input combination that produced the violation — and it must still skip.
    #[test]
    fn a_human_decision_survives_a_moved_head() {
        assert_eq!(vet_action(false, true, false), VetAction::SkipHuman);
        assert_eq!(vet_action(false, true, true), VetAction::SkipHuman);
        assert_eq!(vet_action(true, true, false), VetAction::SkipHuman);
    }

    // --- vet_priority: closest-to-merge first ---------------------------------------------------

    #[test]
    fn green_and_mergeable_vets_first_red_last() {
        let mut order = [
            ("red", vet_priority(Ci::Red, Merge::Mergeable)),
            ("pending", vet_priority(Ci::Pending, Merge::Mergeable)),
            (
                "green-conflicting",
                vet_priority(Ci::Green, Merge::Conflicting),
            ),
            (
                "nochecks-mergeable",
                vet_priority(Ci::NoChecks, Merge::Mergeable),
            ),
            ("green-mergeable", vet_priority(Ci::Green, Merge::Mergeable)),
        ];
        order.sort_by_key(|(_, p)| *p);
        assert_eq!(
            order.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![
                "green-mergeable",
                "nochecks-mergeable",
                "green-conflicting",
                "pending",
                "red"
            ]
        );
        // a green+UNKNOWN-mergeability PR still outranks pending/red (it may just be unsettled).
        assert!(
            vet_priority(Ci::Green, Merge::Unknown) < vet_priority(Ci::Pending, Merge::Mergeable)
        );
    }

    // --- unvetted_row: the per-candidate struct #59 asks for ------------------------------------

    #[test]
    fn row_reports_every_field_and_vets_an_unvetted_pr() {
        let detail = json!({
            "headRefOid": "abc123",
            "labels": [{"name": "ai:reject"}],
            "reviewDecision": "",
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "SUCCESS"}],
            "comments": [],
            "isDraft": false,
        });
        let (action, prio, row) =
            unvetted_row("o/r", 7, "https://github.com/o/r/pull/7", "t", &detail);
        assert_eq!(action, VetAction::Vet);
        assert_eq!(prio, 0);
        assert_eq!(row["pr"], json!("o/r#7"));
        assert_eq!(row["headRefOid"], json!("abc123"));
        assert_eq!(row["labels"], json!(["ai:reject"]));
        assert_eq!(row["reviewDecision"], Value::Null); // empty string normalises to null
        assert_eq!(row["humanSacred"], json!(false));
        assert_eq!(row["vettedAtHead"], json!(false));
        assert_eq!(row["ci"], json!("green"));
        assert_eq!(row["mergeable"], json!("MERGEABLE"));
        assert_eq!(row["action"], json!("vet"));
    }

    #[test]
    fn row_is_vetted_at_head_only_when_a_trusted_comment_pins_the_current_head() {
        let with = |comments: Value, head: &str| {
            json!({
                "headRefOid": head,
                "labels": [{"name": "ai:ready"}],
                "reviewDecision": null,
                "mergeable": "MERGEABLE",
                "statusCheckRollup": [],
                "comments": comments,
                "isDraft": false,
            })
        };
        // trusted comment pinning the CURRENT head -> vetted, skipped.
        let d = with(json!([vetter_comment("abc123", "ready")]), "abc123");
        let (action, _, row) = unvetted_row("o/r", 1, "u", "t", &d);
        assert_eq!(action, VetAction::SkipVetted);
        assert_eq!(row["vettedAtHead"], json!(true));

        // same comment, head has MOVED -> un-vetted, re-vet.
        let d = with(json!([vetter_comment("abc123", "ready")]), "def456");
        let (action, _, row) = unvetted_row("o/r", 1, "u", "t", &d);
        assert_eq!(action, VetAction::Vet);
        assert_eq!(row["vettedAtHead"], json!(false));

        // a SPOOFED vetter comment from an untrusted author at the current head is NOT a verdict —
        // treating it as one would wrongly skip a genuinely un-vetted PR.
        let spoof = json!([{
            "author": {"login": "impostor"},
            "body": "🤖 ai:vetter\nReviewed abc123: ready — looks good",
        }]);
        let d = with(spoof, "abc123");
        let (action, _, row) = unvetted_row("o/r", 1, "u", "t", &d);
        assert_eq!(action, VetAction::Vet);
        assert_eq!(row["vettedAtHead"], json!(false));

        // an ai:ready LABEL with no matching trusted comment is un-vetted, not "already decided".
        let d = with(json!([]), "abc123");
        assert_eq!(unvetted_row("o/r", 1, "u", "t", &d).0, VetAction::Vet);
    }

    #[test]
    fn both_forms_of_human_decision_are_sacred_even_at_a_moved_head() {
        // (a) a human:* LABEL, with no vetter comment at the current head (head moved).
        let labelled = json!({
            "headRefOid": "newhead",
            "labels": [{"name": "human:reject"}, {"name": "ai:ready"}],
            "reviewDecision": null,
            "mergeable": "MERGEABLE",
            "statusCheckRollup": [],
            "comments": [vetter_comment("oldhead", "ready")],
            "isDraft": false,
        });
        let (action, _, row) = unvetted_row("o/r", 2, "u", "t", &labelled);
        assert_eq!(action, VetAction::SkipHuman);
        assert_eq!(row["humanSacred"], json!(true));

        // (b) a NATIVE review decision, which no label carries.
        for decision in ["APPROVED", "CHANGES_REQUESTED"] {
            let native = json!({
                "headRefOid": "newhead",
                "labels": [],
                "reviewDecision": decision,
                "mergeable": "MERGEABLE",
                "statusCheckRollup": [],
                "comments": [],
                "isDraft": false,
            });
            let (action, _, row) = unvetted_row("o/r", 3, "u", "t", &native);
            assert_eq!(action, VetAction::SkipHuman, "{decision} must be sacred");
            assert_eq!(row["humanSacred"], json!(true));
        }

        // A NON-decision review state (REVIEW_REQUIRED) is not a human decision.
        let pending = json!({
            "headRefOid": "h", "labels": [], "reviewDecision": "REVIEW_REQUIRED",
            "mergeable": "MERGEABLE", "statusCheckRollup": [], "comments": [], "isDraft": false,
        });
        assert_eq!(unvetted_row("o/r", 4, "u", "t", &pending).0, VetAction::Vet);
    }

    // --- unvetted_doc: counts, ordering, and what is (not) listed -------------------------------

    #[test]
    fn doc_lists_only_vet_rows_in_vet_first_order_and_counts_the_rest() {
        let row = |pr: &str, action: VetAction, prio: u8| {
            (action, prio, json!({"pr": pr, "action": action.as_str()}))
        };
        let rows = vec![
            row("o/r#3", VetAction::Vet, 4), // red
            row("o/r#1", VetAction::SkipDraft, 4),
            row("o/r#4", VetAction::Vet, 0), // green + mergeable -> first
            row("o/r#5", VetAction::SkipHuman, 4),
            row("o/r#6", VetAction::SkipVetted, 4),
            row("o/r#2", VetAction::Vet, 0), // ties break on the pr key
        ];
        let doc = unvetted_doc(&rows, false, None);
        assert_eq!(doc["counts"]["open"], json!(6));
        assert_eq!(doc["counts"]["vet"], json!(3));
        assert_eq!(doc["counts"]["skipDraft"], json!(1));
        assert_eq!(doc["counts"]["skipHumanDecided"], json!(1));
        assert_eq!(doc["counts"]["skipVettedAtHead"], json!(1));
        let order: Vec<&str> = doc["prs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["pr"].as_str().unwrap())
            .collect();
        assert_eq!(order, vec!["o/r#2", "o/r#4", "o/r#3"]);
        // skipped PRs cost context and need no reasoning -> absent unless asked for.
        assert!(doc.get("skipped").is_none());

        let doc = unvetted_doc(&rows, true, None);
        assert_eq!(doc["skipped"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn empty_state_load_is_a_well_formed_empty_doc() {
        let doc = unvetted_doc(&[], false, None);
        assert_eq!(doc["counts"]["open"], json!(0));
        assert_eq!(doc["counts"]["vet"], json!(0));
        assert_eq!(doc["prs"], json!([]));
        // Present-and-empty, not absent: "nothing was withheld for threads" is an answer.
        assert_eq!(doc["openThreads"], json!([]));
        assert_eq!(doc["more"], json!(0));
    }

    // --- #78: the state-load is bounded BY CONSTRUCTION -----------------------------------------

    /// The vet queue at LIVE scale. Every number here is measured, not invented: the state-load the
    /// vetter's harness refused on 2026-07-27 12:44Z (spill
    /// `mcp-fsm-unvetted-1785156423098.txt`, 63,742 chars on ONE line) reported
    /// `{"open":170,"vet":20,"skipDraft":1,"skipHumanDecided":36,"skipVettedAtHead":113}`, and its
    /// rows averaged 373 bytes. Rows are built through `unvetted_row`, so the widths are the
    /// production emitter's, not a fixture's guess — a row that grows a field grows this test too.
    fn live_scale_rows() -> Vec<(VetAction, u8, Value)> {
        // Slugs/titles at the live repos' actual lengths; a short-name fixture would understate the
        // payload by ~30% and let an unbounded doc slip under the budget.
        let detail = |head: &str, labels: Value, review: Value, draft: bool| {
            json!({
                "headRefOid": head,
                "labels": labels,
                "reviewDecision": review,
                "mergeable": "MERGEABLE",
                "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "SUCCESS"}],
                "comments": [],
                "isDraft": draft,
            })
        };
        let mut rows = Vec::new();
        let mut push = |n: u64, labels: Value, review: Value, draft: bool, vetted: bool| {
            let slug = "rainlanguage/rain.orderbook.interface";
            let head = "58a938eec90fa7fb5c864cb58934335475d29e7e";
            let mut d = detail(head, labels, review, draft);
            if vetted {
                d["comments"] = json!([vetter_comment(head, "ready")]);
            }
            rows.push(unvetted_row(
                slug,
                n,
                &format!("https://github.com/{slug}/pull/{n}"),
                "Replace the hand-rolled float parser with LibDecimalFloat (#2444)",
                &d,
            ));
        };
        for n in 0..20 {
            push(1000 + n, json!([]), Value::Null, false, false);
        }
        for n in 0..113 {
            push(
                2000 + n,
                json!([{"name": "ai:ready"}]),
                Value::Null,
                false,
                true,
            );
        }
        for n in 0..36 {
            push(
                3000 + n,
                json!([{"name": "human:reject"}]),
                Value::Null,
                false,
                false,
            );
        }
        push(4000, json!([]), Value::Null, true, false);
        rows
    }

    // THE regression test for #78. At the live queue size the state-load handed to the vetter must
    // fit the budget one tool result has — including `include_skipped`, which is the exact call
    // that returned 63,742 characters and was refused. The bound is structural (a page), so it does
    // not depend on how many PRs happen to be open.
    #[test]
    fn the_state_load_is_bounded_at_the_live_queue_size() {
        let rows = live_scale_rows();
        assert_eq!(rows.len(), 170, "the fixture is the live queue size");

        // The fixture reproduces the defect, checked against the ROWS rather than the emitter so it
        // cannot be satisfied by the fix it is testing: listing all 170 of these rows in full — the
        // pre-#78 shape — is over budget on its own, before any wrapper. Without this the bounded
        // assertion below could pass on a toy queue.
        let dumped: usize = rows.iter().map(|(_, _, r)| r.to_string().len()).sum();
        assert!(
            dumped > MCP_MAX_RESULT_BYTES,
            "fixture must reproduce the over-budget payload (rows total {dumped} bytes)"
        );

        for include_skipped in [false, true] {
            let doc = unvetted_doc(&rows, include_skipped, Some(STATE_LOAD_PAGE_DEFAULT));
            let s = doc.to_string();
            assert!(
                s.len() <= MCP_MAX_RESULT_BYTES,
                "state-load (include_skipped={include_skipped}) is {} bytes, over the {MCP_MAX_RESULT_BYTES}-byte budget",
                s.len()
            );
            // Whole-queue truth survives the page: the counts are never the page's size.
            assert_eq!(doc["counts"]["open"], json!(170));
            assert_eq!(doc["counts"]["vet"], json!(20));
            assert_eq!(doc["counts"]["skipVettedAtHead"], json!(113));
            // Every list is capped, and what it left behind is STATED rather than inferable.
            assert_eq!(
                doc["prs"].as_array().unwrap().len(),
                STATE_LOAD_PAGE_DEFAULT
            );
            assert_eq!(doc["more"], json!(10));
            if include_skipped {
                assert_eq!(
                    doc["skipped"].as_array().unwrap().len(),
                    STATE_LOAD_PAGE_DEFAULT
                );
                assert_eq!(doc["moreSkipped"], json!(140));
            }
        }

        // The ceiling is what makes the bound structural rather than a hopeful default: the LARGEST
        // page a caller may ask for still fits, so no argument reachable through the guard can
        // reproduce the failure.
        let max = *STATE_LOAD_PAGE_RANGE.end() as usize;
        assert!(
            unvetted_doc(&rows, true, Some(max)).to_string().len() <= MCP_MAX_RESULT_BYTES,
            "the maximum page must still fit the budget"
        );

        // …and it holds as the queue GROWS, which is the property a byte check at today's size
        // cannot give: at 5x the live open-PR count the page is the same size, because the page is
        // what is bounded, not the queue.
        let grown: Vec<_> = std::iter::repeat_with(live_scale_rows)
            .take(5)
            .flatten()
            .collect();
        assert_eq!(grown.len(), 850);
        let doc = unvetted_doc(&grown, true, Some(max)).to_string();
        assert!(
            doc.len() <= MCP_MAX_RESULT_BYTES,
            "a 5x queue must not change the page size (got {} bytes)",
            doc.len()
        );
    }

    // #78 requirement 1: the open-threads accounting (#2) must reach the vetter WITHOUT depending on
    // an optional argument. It lived only in the skipped list, the skipped list is what blew the
    // budget, and the vetter's fallback dropped it — so the gate merged hours earlier was invisible
    // to its only consumer.
    #[test]
    fn the_open_threads_accounting_reaches_the_vetter_unconditionally() {
        let gated = |pr: &str, threads: Value| {
            (
                VetAction::SkipOpenThreads,
                0,
                json!({
                    "pr": pr,
                    "url": format!("https://github.com/{}", pr.replace('#', "/pull/")),
                    "action": "skip-open-threads",
                    "unresolvedThreads": threads,
                }),
            )
        };
        let rows = vec![
            (VetAction::Vet, 0, json!({"pr": "o/r#1", "action": "vet"})),
            gated("o/r#2", json!(3)),
            // an UNREADABLE thread state is fail-closed too, and must stay distinguishable from a
            // verified zero once it reaches the vetter.
            gated("o/r#3", Value::Null),
            (
                VetAction::SkipVetted,
                4,
                json!({"pr": "o/r#4", "action": "skip-vetted-at-head"}),
            ),
        ];

        // No `include_skipped`, default page — the call the prompt actually tells the vetter to make.
        let doc = unvetted_doc(&rows, false, Some(STATE_LOAD_PAGE_DEFAULT));
        assert_eq!(doc["counts"]["skipOpenThreads"], json!(2));
        assert_eq!(
            doc["openThreads"],
            json!([
                {"pr": "o/r#2", "url": "https://github.com/o/r/pull/2", "unresolvedThreads": 3},
                {"pr": "o/r#3", "url": "https://github.com/o/r/pull/3", "unresolvedThreads": null},
            ]),
            "the withheld PRs and their thread counts reach the vetter with no opt-in"
        );
        assert_eq!(doc["moreOpenThreads"], json!(0));
        // and only the thread-gated rows are there — a vetted-at-head PR is not "withheld work".
        assert!(!doc["openThreads"].to_string().contains("o/r#4"));

        // The digest carried by `include_skipped` keeps the per-row thread count too, at ~1/5 the
        // bytes of the full row: it is the part of a skipped row a caller can act on.
        let doc = unvetted_doc(&rows, true, Some(STATE_LOAD_PAGE_DEFAULT));
        assert_eq!(
            doc["skipped"],
            json!([
                {"pr": "o/r#2", "action": "skip-open-threads", "unresolvedThreads": 3},
                {"pr": "o/r#3", "action": "skip-open-threads", "unresolvedThreads": null},
                {"pr": "o/r#4", "action": "skip-vetted-at-head"},
            ])
        );
    }

    // The page must not silently re-order or drop the queue's head: a page is the FIRST n in
    // vet-first order, and `more` is exactly the remainder.
    #[test]
    fn a_page_is_the_front_of_the_queue_and_more_is_the_remainder() {
        let rows: Vec<_> = (0..7)
            .map(|n| {
                (
                    VetAction::Vet,
                    n as u8,
                    json!({"pr": format!("o/r#{n}"), "action": "vet"}),
                )
            })
            .collect();
        let doc = unvetted_doc(&rows, false, Some(3));
        let listed: Vec<&str> = doc["prs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["pr"].as_str().unwrap())
            .collect();
        assert_eq!(listed, vec!["o/r#0", "o/r#1", "o/r#2"]);
        assert_eq!(doc["more"], json!(4));
        assert_eq!(doc["counts"]["vet"], json!(7), "counts stay whole-queue");
        // a page larger than the queue leaves nothing behind.
        assert_eq!(unvetted_doc(&rows, false, Some(99))["more"], json!(0));
        assert_eq!(unvetted_doc(&rows, false, None)["more"], json!(0));
    }

    // --- pr_context_doc: the one-call review bundle ---------------------------------------------

    #[test]
    fn truncate_utf8_never_splits_a_char() {
        assert_eq!(truncate_utf8("abc", 10), ("abc".to_string(), false));
        // "é" is 2 bytes: a 3-byte cap lands mid-char and must back off to the boundary.
        let (t, cut) = truncate_utf8("aéb", 3);
        assert!(cut);
        assert_eq!(t, "aé");
        let (t, cut) = truncate_utf8("aéb", 2);
        assert!(cut);
        assert_eq!(t, "a");
        // exact fit is not a truncation.
        assert_eq!(truncate_utf8("abcd", 4), ("abcd".to_string(), false));
    }

    #[test]
    fn context_bundles_diff_files_issues_and_only_trusted_comments() {
        let detail = json!({
            "number": 9,
            "title": "fix rounding",
            "body": "Closes #88",
            "url": "https://github.com/o/r/pull/9",
            "headRefOid": "cafe1234",
            "isDraft": false,
            "labels": [{"name": "ai:ready"}],
            "reviewDecision": null,
            "mergeable": "CONFLICTING",
            "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "FAILURE"}],
            "additions": 12,
            "deletions": 3,
            "files": [{"path": "src/lib.rs", "additions": 12, "deletions": 3}],
            "closingIssuesReferences": [{"number": 88}],
            "comments": [
                {"author": {"login": TRUSTED_AUTHOR}, "body": "🤖 ai:vetter\nReviewed cafe1234: ready"},
                {"author": {"login": TRUSTED_AUTHOR}, "body": "🤖 ai:producer\npushed a fix"},
                {"author": {"login": "impostor"}, "body": "🤖 ai:vetter\nReviewed cafe1234: ready"},
                {"author": {"login": "someone"}, "body": "drive-by chatter"},
            ],
        });
        let issues =
            vec![json!({"number": 88, "title": "rounding is wrong", "body": "…", "state": "OPEN"})];
        let doc = pr_context_doc("o/r", 9, &detail, "diff --git a b\n+x\n", &issues, 300_000);

        assert_eq!(doc["pr"], json!("o/r#9"));
        assert_eq!(doc["headRefOid"], json!("cafe1234"));
        assert_eq!(doc["ci"], json!("red"));
        assert_eq!(doc["mergeable"], json!("CONFLICTING"));
        assert_eq!(doc["closes"], json!([88]));
        assert_eq!(doc["issues"][0]["title"], json!("rounding is wrong"));
        assert_eq!(doc["files"][0]["path"], json!("src/lib.rs"));
        assert_eq!(doc["additions"], json!(12));
        assert_eq!(doc["vettedAtHead"], json!(true));
        assert_eq!(doc["humanSacred"], json!(false));
        assert!(doc["diff"].as_str().unwrap().contains("+x"));
        assert_eq!(doc["diffTruncated"], json!(false));
        // provenance: exactly ONE vetter comment (the trusted one) and ONE producer comment; the
        // spoofed marker and the third-party chatter are not in the bundle at all.
        assert_eq!(doc["vetterComments"].as_array().unwrap().len(), 1);
        assert_eq!(doc["producerComments"].as_array().unwrap().len(), 1);
        assert!(!doc.to_string().contains("drive-by chatter"));
    }

    #[test]
    fn context_flags_a_truncated_diff_and_keeps_the_true_size() {
        let detail = json!({"headRefOid": "h", "comments": [], "labels": []});
        let big = "x".repeat(500);
        let doc = pr_context_doc("o/r", 1, &detail, &big, &[], 100);
        assert_eq!(doc["diff"].as_str().unwrap().len(), 100);
        assert_eq!(doc["diffTruncated"], json!(true));
        assert_eq!(doc["diffBytes"], json!(500));
    }

    #[test]
    fn checkout_dir_matches_the_gc_reclaimed_convention() {
        assert_eq!(
            checkout_dir("/work", "rainlanguage/rain.flare", 170),
            "/work/vet-rain.flare-170"
        );
        assert_eq!(checkout_dir("/work/", "o/r", 1), "/work/vet-r-1");
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;
    use serde_json::json;

    /// A recording fake for the effectful half: every VALIDATED call lands here, so a test can assert
    /// both what reached the effect and — crucially — what did NOT.
    struct FakeExec {
        calls: std::cell::RefCell<Vec<McpCall>>,
        reply: Result<String, String>,
        profile: McpProfile,
        roots: Vec<String>,
    }

    impl FakeExec {
        fn ok() -> Self {
            FakeExec {
                calls: std::cell::RefCell::new(Vec::new()),
                reply: Ok("{\"ok\":true}".to_string()),
                profile: McpProfile::Vetter,
                roots: vec!["/work".to_string()],
            }
        }
        fn failing(msg: &str) -> Self {
            FakeExec {
                reply: Err(msg.to_string()),
                ..FakeExec::ok()
            }
        }
        fn producer() -> Self {
            FakeExec {
                profile: McpProfile::Producer,
                ..FakeExec::ok()
            }
        }
        fn with_roots(mut self, roots: &[&str]) -> Self {
            self.roots = roots.iter().map(|r| r.to_string()).collect();
            self
        }
        fn handle(&self, req: &Value) -> Option<Value> {
            let mut f = |c: McpCall| {
                self.calls.borrow_mut().push(c);
                self.reply.clone()
            };
            mcp_handle(self.profile, &self.roots, req, &mut f)
        }
        fn calls(&self) -> Vec<McpCall> {
            self.calls.borrow().iter().cloned().collect()
        }
    }

    fn call(name: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": name, "arguments": args}})
    }

    fn is_error(resp: &Value) -> bool {
        resp["result"]["isError"].as_bool().unwrap_or(false)
    }

    fn text(resp: &Value) -> String {
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    // --- handshake ------------------------------------------------------------------------------

    #[test]
    fn initialize_negotiates_a_version_the_client_knows() {
        // A supported request is echoed back verbatim.
        assert_eq!(mcp_protocol_version(Some("2024-11-05")), "2024-11-05");
        assert_eq!(mcp_protocol_version(Some("2025-11-25")), "2025-11-25");
        // An unknown/absent revision falls back to one we speak — never to the client's unknown
        // string, which is what makes a client abort the handshake.
        assert_eq!(
            mcp_protocol_version(Some("1999-01-01")),
            MCP_PROTOCOL_DEFAULT
        );
        assert_eq!(mcp_protocol_version(None), MCP_PROTOCOL_DEFAULT);
        assert!(MCP_PROTOCOL_SUPPORTED.contains(&MCP_PROTOCOL_DEFAULT));
    }

    #[test]
    fn initialize_advertises_tools_and_identity() {
        let f = FakeExec::ok();
        let resp = f
            .handle(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                            "params": {"protocolVersion": "2025-06-18"}}))
            .expect("initialize is a request, not a notification");
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(0));
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-06-18"));
        // the tools capability must be advertised or no client ever calls tools/list.
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("fsm"));
    }

    #[test]
    fn a_notification_is_never_answered() {
        let f = FakeExec::ok();
        // `notifications/initialized` carries no id; replying to it is a protocol violation.
        assert!(f
            .handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none());
        assert!(f
            .handle(&json!({"jsonrpc": "2.0", "id": null, "method": "notifications/cancelled"}))
            .is_none());
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error_not_a_tool_result() {
        let f = FakeExec::ok();
        let resp = f
            .handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}))
            .unwrap();
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert!(resp.get("result").is_none());
    }

    // --- the surface itself ---------------------------------------------------------------------

    #[test]
    fn tools_list_is_exactly_the_vetter_fsm_surface() {
        let f = FakeExec::ok();
        let resp = f
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "unvetted",
                "pr_context",
                "pr_checkout",
                "record_verdict",
                // `pr_checkout` creates a clone, so the vetter owns the move that disposes of it.
                "clone_release",
                // The second subject: producer close-candidate flags on issues (#72).
                "unvetted_close_candidates",
                "close_candidate_context",
                "record_close_candidate_verdict",
            ]
        );
        // Every tool is callable: a name, a one-line description, an object schema.
        for t in &tools {
            assert!(!t["description"].as_str().unwrap().is_empty());
            assert_eq!(t["inputSchema"]["type"], json!("object"));
        }
        // The surface stays SMALL on purpose (#52: schemas ride in every request's preamble).
        // Two subjects × (state-load, read one, record one verdict), plus clone_release.
        assert!(tools.len() <= 9, "keep the tool surface small");

        // The close-candidate write is constrained by SCHEMA, so the prompt cannot invent a
        // verdict and the vetter cannot reach for a `human:*` disposition.
        let cc = tools
            .iter()
            .find(|t| t["name"] == json!("record_close_candidate_verdict"))
            .unwrap();
        let cc_verdicts: Vec<&str> = cc["inputSchema"]["properties"]["verdict"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Pinned to the vocabulary itself, so the schema and CC_VERDICTS cannot drift apart.
        assert_eq!(cc_verdicts, CC_VERDICTS.to_vec());

        let rv = tools
            .iter()
            .find(|t| t["name"] == json!("record_verdict"))
            .unwrap();
        let required: Vec<&str> = rv["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // cost is REQUIRED by the schema — the prompt could only ask for it.
        assert!(required.contains(&"cost"));
        assert!(required.contains(&"verdict"));
        let verdicts: Vec<&str> = rv["inputSchema"]["properties"]["verdict"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(verdicts, VETTER_VERDICTS.to_vec());
    }

    // --- pr refs --------------------------------------------------------------------------------

    #[test]
    fn pr_refs_parse_only_in_owner_repo_number_form() {
        assert_eq!(
            parse_pr_ref("rainlanguage/rain.flare#170"),
            Ok(("rainlanguage/rain.flare".to_string(), 170))
        );
        assert_eq!(parse_pr_ref("  o/r#1  "), Ok(("o/r".to_string(), 1)));
        for bad in [
            "rain.flare#170", // no owner — the org must never be guessed
            "o/r",            // no number
            "o/r#",
            "o/r#abc",
            "o/r#0", // PR numbers start at 1
            "o/r#-1",
            "a/b/c#1", // not a slug
            "/r#1",
            "o/#1",
            "",
        ] {
            assert!(parse_pr_ref(bad).is_err(), "{bad:?} must not parse");
        }
    }

    // --- the transition guard -------------------------------------------------------------------

    #[test]
    fn a_verdict_outside_the_vocabulary_is_refused_and_never_reaches_the_effect() {
        let f = FakeExec::ok();
        for bogus in ["approve", "merge", "close-candidate", "READY", "ready!", ""] {
            let resp = f
                .handle(&call(
                    "record_verdict",
                    json!({"pr": "o/r#1", "verdict": bogus, "note": "n", "cost": 10, "basis": "b"}),
                ))
                .unwrap();
            assert!(is_error(&resp), "{bogus:?} must be refused");
        }
        // The refusal is structural: nothing reached the write path.
        assert!(f.calls().is_empty());
    }

    #[test]
    fn record_verdict_requires_a_scored_cost_a_note_and_a_short_basis() {
        let f = FakeExec::ok();
        let base = |extra: Value| {
            let mut m = json!({"pr": "o/r#1", "verdict": "ready", "note": "closes #88 — pinned by test", "cost": 300, "basis": "small diff"});
            for (k, v) in extra.as_object().unwrap() {
                m.as_object_mut().unwrap().insert(k.clone(), v.clone());
            }
            m
        };
        // cost is mandatory and 0-1000.
        for bad in [
            json!({"cost": null}),
            json!({"cost": 1001}),
            json!({"cost": -1}),
            json!({"cost": "300"}),
        ] {
            let resp = f
                .handle(&call("record_verdict", base(bad.clone())))
                .unwrap();
            assert!(is_error(&resp), "cost {bad} must be refused");
        }
        // a note that says nothing is refused; so is a basis that is a paragraph.
        assert!(is_error(
            &f.handle(&call("record_verdict", base(json!({"note": "   "}))))
                .unwrap()
        ));
        assert!(is_error(&f
            .handle(&call(
                "record_verdict",
                base(json!({"basis": "one two three four five six seven eight nine ten eleven twelve thirteen"}))
            ))
            .unwrap()));
        assert!(f.calls().is_empty(), "no invalid verdict reached the write");

        // the boundaries themselves are legal.
        for good in [
            json!({"cost": 0}),
            json!({"cost": 1000}),
            json!({"basis": "docs-only"}),
        ] {
            let resp = f
                .handle(&call("record_verdict", base(good.clone())))
                .unwrap();
            assert!(!is_error(&resp), "{good} must be accepted");
        }
        assert_eq!(f.calls().len(), 3);
    }

    #[test]
    fn a_valid_verdict_reaches_the_write_exactly_as_given() {
        let f = FakeExec::ok();
        let resp = f
            .handle(&call(
                "record_verdict",
                json!({"pr": "cyclofinance/cyclo.site#369", "verdict": "reject", "note": "closes #12 — no discriminating test", "cost": 640, "basis": "accounting path"}),
            ))
            .unwrap();
        assert!(!is_error(&resp));
        assert_eq!(
            f.calls(),
            vec![McpCall::RecordVerdict {
                slug: "cyclofinance/cyclo.site".to_string(),
                num: 369,
                verdict: "reject".to_string(),
                note: "closes #12 — no discriminating test".to_string(),
                cost: 640,
                basis: "accounting path".to_string(),
            }]
        );
    }

    // The human-sacred backstop lives in the shared write path (verdict_plan → RefuseHuman, exit 3);
    // the MCP layer must SURFACE that refusal to the model rather than swallow it into a success.
    #[test]
    fn a_human_decided_pr_refusal_comes_back_as_a_tool_error() {
        // the guard itself, on the JSON the write path reads:
        let human = json!({"labels": [{"name": "human:reject"}], "comments": [], "headRefOid": "h", "reviewDecision": null});
        assert_eq!(
            verdict_plan(&human, "ai:ready", "ready"),
            VerdictPlan::RefuseHuman
        );
        let approved =
            json!({"labels": [], "comments": [], "headRefOid": "h", "reviewDecision": "APPROVED"});
        assert_eq!(
            verdict_plan(&approved, "ai:ready", "ready"),
            VerdictPlan::RefuseHuman
        );

        // …and its surfacing:
        let f = FakeExec::failing("human verdict present on o/r#1; not overriding [exit 3]");
        let resp = f
            .handle(&call(
                "record_verdict",
                json!({"pr": "o/r#1", "verdict": "ready", "note": "n", "cost": 1, "basis": "b"}),
            ))
            .unwrap();
        assert!(is_error(&resp));
        assert!(text(&resp).contains("not overriding"));
    }

    #[test]
    fn tools_outside_the_surface_do_not_exist() {
        let f = FakeExec::ok();
        for name in [
            "merge",
            "gh",
            "record-verdict", // the CLI spelling is not the tool name
            "flag_close_candidate",
            "worklist",
            "",
            // the producer's clone-management tools are not the vetter's moves
            "clone_create",
            "clone_gc",
            "clone_list",
        ] {
            let resp = f.handle(&call(name, json!({}))).unwrap();
            assert!(is_error(&resp), "{name:?} must not exist");
            assert!(
                text(&resp).contains("unvetted"),
                "the error names the real surface"
            );
        }
        assert!(f.calls().is_empty());
    }

    #[test]
    fn read_tools_validate_their_arguments() {
        let f = FakeExec::ok();
        // a missing/ill-formed pr ref never reaches a fetch.
        assert!(is_error(&f.handle(&call("pr_context", json!({}))).unwrap()));
        assert!(is_error(
            &f.handle(&call("pr_context", json!({"pr": "r#1"}))).unwrap()
        ));
        assert!(is_error(
            &f.handle(&call("pr_checkout", json!({"pr": 12}))).unwrap()
        ));
        // an absurd diff cap is refused rather than silently clamped.
        assert!(is_error(
            &f.handle(&call(
                "pr_context",
                json!({"pr": "o/r#1", "max_diff_bytes": 0})
            ))
            .unwrap()
        ));
        assert!(is_error(
            &f.handle(&call(
                "pr_context",
                json!({"pr": "o/r#1", "max_diff_bytes": 99_000_000u64})
            ))
            .unwrap()
        ));
        assert!(f.calls().is_empty());

        // defaults: no cap given -> the documented default; unvetted lists only what needs vetting.
        f.handle(&call("pr_context", json!({"pr": "o/r#1"})))
            .unwrap();
        f.handle(&call("unvetted", json!({}))).unwrap();
        f.handle(&call("unvetted", json!({"include_skipped": true})))
            .unwrap();
        assert_eq!(
            f.calls(),
            vec![
                McpCall::PrContext {
                    slug: "o/r".to_string(),
                    num: 1,
                    max_diff_bytes: DEFAULT_MAX_DIFF_BYTES
                },
                McpCall::Unvetted {
                    include_skipped: false,
                    limit: STATE_LOAD_PAGE_DEFAULT
                },
                McpCall::Unvetted {
                    include_skipped: true,
                    limit: STATE_LOAD_PAGE_DEFAULT
                },
            ]
        );
    }

    // #78: a state-load handed to a token-budgeted caller is ALWAYS paged. An out-of-range page is
    // refused rather than clamped — a clamp leaves the caller believing it got what it asked for.
    #[test]
    fn a_state_load_page_is_bounded_by_the_transition_guard() {
        let f = FakeExec::ok();
        for tool in ["unvetted", "unvetted_close_candidates"] {
            for bad in [json!(0), json!(26), json!("10"), json!(-1), json!(1000)] {
                let resp = f.handle(&call(tool, json!({"limit": bad}))).unwrap();
                assert!(is_error(&resp), "{tool} limit={bad} must be refused");
                assert!(text(&resp).contains("limit must be an integer in 1..=25"));
            }
        }
        assert!(f.calls().is_empty(), "no refused page reached a fetch");

        // …and an in-range page is carried through verbatim.
        f.handle(&call("unvetted", json!({"limit": 3}))).unwrap();
        f.handle(&call("unvetted_close_candidates", json!({"limit": 25})))
            .unwrap();
        assert_eq!(
            f.calls(),
            vec![
                McpCall::Unvetted {
                    include_skipped: false,
                    limit: 3
                },
                McpCall::UnvettedCloseCandidates {
                    include_skipped: false,
                    limit: 25
                },
            ]
        );
    }

    // #78, the load-bearing one: an over-budget result is THIS SERVER's error. On 2026-07-27 the
    // server handed back 63,742 bytes, the harness refused it, and the vetter improvised a fallback
    // that silently dropped the open-threads accounting. A tool that cannot answer within budget
    // must SAY SO, so the caller's only available next move is a narrower call.
    #[test]
    fn an_over_budget_result_is_refused_by_the_server_not_left_to_the_caller() {
        let huge = "x".repeat(MCP_MAX_RESULT_BYTES + 1);
        let f = FakeExec {
            reply: Ok(huge.clone()),
            ..FakeExec::ok()
        };
        let resp = f.handle(&call("unvetted", json!({}))).unwrap();
        assert!(
            is_error(&resp),
            "an over-budget state-load must be an error"
        );
        let t = text(&resp);
        assert!(t.contains("unvetted"), "the error names the tool: {t}");
        assert!(
            t.contains(&format!("{}", MCP_MAX_RESULT_BYTES + 1))
                && t.contains(&format!("{MCP_MAX_RESULT_BYTES}")),
            "the error states the actual size and the budget: {t}"
        );
        assert!(
            t.contains("limit"),
            "the error names the argument that makes the call smaller: {t}"
        );
        // NOT a truncation and NOT a spill: no fragment of the payload is handed back to be
        // half-read. A partial state-load cannot say what it is missing.
        assert!(!t.contains(&huge[..64]), "no payload fragment is returned");

        // Exactly at the budget is fine — the boundary is `>`, not `>=`.
        let exact = FakeExec {
            reply: Ok("x".repeat(MCP_MAX_RESULT_BYTES)),
            ..FakeExec::ok()
        };
        assert!(!is_error(
            &exact.handle(&call("unvetted", json!({}))).unwrap()
        ));

        // `pr_context` gets the SAME budget, and that is the fix (#81). It used to be budgeted at
        // `max_diff_bytes + MCP_MAX_RESULT_BYTES`, so this very payload was "within budget" for a
        // large enough argument — up to 332,000 bytes, six times what the harness accepts.
        let resp = f
            .handle(&call(
                "pr_context",
                json!({"pr": "o/r#1", "max_diff_bytes": MAX_MAX_DIFF_BYTES}),
            ))
            .unwrap();
        assert!(
            is_error(&resp),
            "no argument may buy a pr_context more room than any other tool gets"
        );
        assert!(text(&resp).contains("max_diff_bytes"));
    }

    // The ordering that is the whole mechanism: OUR guard must fire before the harness's. The
    // relationship between the budget and the MEASURED gate is a compile-time assertion beside the
    // constants (raise the budget past the gate and this crate does not build); this is the runtime
    // half, which also covers what a REAL document does — a constant can be right while the thing
    // built from it is not. `black_box` keeps the comparison out of const-folding so the assertion
    // is genuinely evaluated here.
    #[test]
    fn the_result_budget_stays_under_the_measured_harness_gate() {
        use std::hint::black_box;
        let (budget, gate) = (
            black_box(MCP_MAX_RESULT_BYTES),
            black_box(MEASURED_HARNESS_GATE_BYTES),
        );
        assert!(
            budget < gate,
            "budget {budget} is not below the gate {gate}"
        );
        assert!(
            budget * 4 <= gate * 3,
            "keep 25% margin: {budget} vs {gate}"
        );
        // The largest document this tool can be asked for still lands under the gate, not merely
        // under the budget — which is the property the live harness actually checks.
        let (detail, diff) = ctx_fixture(200, 4_000_000);
        let len = fit_pr_context("o/r", 1, &detail, &diff, &[], MAX_MAX_DIFF_BYTES as usize)
            .unwrap()
            .to_string()
            .len();
        assert!(
            len < gate,
            "worst-case document is {len} bytes, gate is {gate}"
        );
    }

    // The property that makes "re-call NARROWER" terminate: the allowance does not move with the
    // request. While `pr_context`'s budget scaled with `max_diff_bytes` and its diff was truncated
    // to `max_diff_bytes`, lowering the argument lowered both sides equally and the caller could
    // loop for ever — so this is pinned as an invariant, not left as a property of one match arm.
    #[test]
    fn the_result_budget_does_not_move_with_any_argument() {
        let calls = [
            McpCall::PrContext {
                slug: "o/r".into(),
                num: 1,
                max_diff_bytes: 1,
            },
            McpCall::PrContext {
                slug: "o/r".into(),
                num: 1,
                max_diff_bytes: MAX_MAX_DIFF_BYTES as usize,
            },
            McpCall::Unvetted {
                include_skipped: true,
                limit: 25,
            },
            McpCall::CloneList,
        ];
        for c in &calls {
            assert_eq!(
                call_result_budget(c),
                MCP_MAX_RESULT_BYTES,
                "every call gets the one budget: {c:?}"
            );
        }
        // And the argument cannot be raised past it, so "ask for more diff than a whole result may
        // occupy" is not expressible.
        assert_eq!(MAX_MAX_DIFF_BYTES as usize, MCP_MAX_RESULT_BYTES);
        let e = validate_call(
            McpProfile::Vetter,
            &[],
            "pr_context",
            &json!({"pr": "o/r#1", "max_diff_bytes": MCP_MAX_RESULT_BYTES + 1}),
        )
        .unwrap_err();
        assert!(e.contains("max_diff_bytes must be an integer in"), "{e}");
    }

    // The advice each refusal gives must be advice that can work. With one fixed budget it is:
    // lowering the named argument lowers the payload against an allowance that does not move.
    #[test]
    fn each_refusal_names_an_argument_that_actually_narrows_it() {
        let too_big = FakeExec {
            reply: Ok("x".repeat(MCP_MAX_RESULT_BYTES + 1_001)),
            ..FakeExec::ok()
        };
        let load = text(&too_big.handle(&call("unvetted", json!({}))).unwrap());
        assert!(load.contains("Re-call NARROWER: lower `limit`."), "{load}");
        let ctx = text(
            &too_big
                .handle(&call(
                    "pr_context",
                    json!({"pr": "o/r#1", "max_diff_bytes": 1_000}),
                ))
                .unwrap(),
        );
        assert!(
            ctx.contains("Re-call NARROWER: lower `max_diff_bytes`."),
            "{ctx}"
        );
    }

    /// A `pr_context` input whose metadata is `meta_bytes`-ish and whose diff is `diff` bytes long.
    fn ctx_fixture(meta_bytes: usize, diff_bytes: usize) -> (Value, String) {
        let detail = json!({
            "url": "https://github.com/o/r/pull/1",
            "title": "t",
            "body": "b".repeat(meta_bytes),
            "headRefOid": "a".repeat(40),
            "additions": 1, "deletions": 1,
            "files": [{"path": "src/lib.rs", "additions": 1, "deletions": 1}],
        });
        (detail, "d".repeat(diff_bytes))
    }

    // THE requirement of #81: no `pr_context` result can exceed the ceiling, whatever
    // `max_diff_bytes` says. Before this, a 300 KB default against a ~50 KB harness ceiling meant the
    // guard never spoke and the harness's untyped replacement arrived instead — `is_error` unset, so
    // every downstream rule about "a tool error is an instruction" stopped applying.
    #[test]
    fn a_pr_context_result_can_never_exceed_the_ceiling() {
        for diff_bytes in [0, 1_000, MCP_MAX_RESULT_BYTES, 4_000_000] {
            for asked in [1, 1_000, MCP_MAX_RESULT_BYTES] {
                let (detail, diff) = ctx_fixture(200, diff_bytes);
                let doc = fit_pr_context("o/r", 1, &detail, &diff, &[], asked).unwrap();
                let len = doc.to_string().len();
                assert!(
                    len <= MCP_MAX_RESULT_BYTES,
                    "diff={diff_bytes} asked={asked} produced {len} bytes, over {MCP_MAX_RESULT_BYTES}"
                );
                // …and it says how much of the diff it actually carried, so "I asked for more than
                // this" is visible rather than inferred.
                assert_eq!(doc["diffBytes"], json!(diff_bytes));
                let included = doc["diffIncluded"].as_u64().unwrap() as usize;
                assert!(included <= asked.min(diff_bytes));
                assert_eq!(doc["diffTruncated"], json!(included < diff_bytes));
            }
        }
    }

    // Convergence, which is what "re-call NARROWER" depends on: a smaller argument is a strictly
    // smaller result, monotonically, because the allowance no longer moves with it.
    #[test]
    fn narrowing_max_diff_bytes_strictly_shrinks_the_result() {
        let (detail, diff) = ctx_fixture(100, 30_000);
        let mut last = usize::MAX;
        for asked in [20_000, 10_000, 5_000, 1_000, 100, 1] {
            let len = fit_pr_context("o/r", 1, &detail, &diff, &[], asked)
                .unwrap()
                .to_string()
                .len();
            assert!(len < last, "asked={asked} gave {len}, not below {last}");
            last = len;
        }
    }

    // The one case no argument fixes, and it must say so rather than hand back a smaller diff nobody
    // asked for: the metadata alone is over the ceiling.
    #[test]
    fn pr_context_metadata_alone_over_the_ceiling_is_a_typed_error() {
        let (detail, diff) = ctx_fixture(MCP_MAX_RESULT_BYTES + 5_000, 10_000);
        let e = fit_pr_context("o/r", 1, &detail, &diff, &[], MCP_MAX_RESULT_BYTES).unwrap_err();
        assert!(e.starts_with("error:"), "{e}");
        assert!(e.contains("with NO diff at all"), "{e}");
        assert!(
            e.contains("cannot shrink it"),
            "it says the argument cannot help: {e}"
        );
        assert!(e.contains("record NO verdict"), "{e}");
    }

    // #81, link 2 of the wrong-tree chain. `pr_checkout`'s contract is "a working tree at path P";
    // when it cannot, the caller must meet a typed refusal, not a sentence it can reason around. The
    // transport half was already right (an `Err` from exec becomes `isError: true` — the live trace
    // of 2026-07-27 shows `is_error: True` on both failed calls), and what was NOT right is that the
    // text was a bare `gh … failed: fatal: cannot set up tracking information`: a git message with no
    // statement of what the caller now has, and no instruction. This pins BOTH halves together,
    // because either alone is what the vetter improvised past.
    #[test]
    fn a_failed_checkout_is_an_error_carrying_the_do_not_substitute_instruction() {
        let f = FakeExec::failing(&checkout_failure_error(
            "rainlanguage/rain.factory#47",
            "/home/gildlab/code/vet-rain.factory-47",
            "git fetch failed: could not read refs/pull/47/head",
        ));
        let resp = f
            .handle(&call(
                "pr_checkout",
                json!({"pr": "rainlanguage/rain.factory#47"}),
            ))
            .unwrap();
        assert!(
            is_error(&resp),
            "a checkout that produced no tree is not a successful call"
        );
        let t = text(&resp);
        assert!(t.contains("There is NO checkout"), "{t}");
        assert!(t.contains("Do NOT search the filesystem"), "{t}");
        assert!(t.contains("record NO verdict"), "{t}");
        // No path is offered, so nothing in the result can be read as "the tree is over there".
        assert!(!t.contains("vet-rain.factory-dep"), "{t}");
    }

    #[test]
    fn an_effect_failure_is_reported_not_swallowed() {
        let f = FakeExec::failing("error: `gh pr diff o/r#1` failed");
        let resp = f
            .handle(&call("pr_context", json!({"pr": "o/r#1"})))
            .unwrap();
        assert!(is_error(&resp));
        assert!(text(&resp).contains("gh pr diff"));
    }

    // --- profiles -------------------------------------------------------------------------------

    #[test]
    fn the_producer_surface_is_clone_lifecycle_and_nothing_else() {
        let f = FakeExec::producer();
        let resp = f
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["clone_create", "clone_release", "clone_list", "clone_gc"]
        );
        for t in &tools {
            assert!(!t["description"].as_str().unwrap().is_empty());
            assert_eq!(t["inputSchema"]["type"], json!("object"));
        }
    }

    // A profile is a real boundary, not a listing cosmetic: the vetter's WRITE must be unreachable
    // from the producer's server even when it is named directly.
    #[test]
    fn a_profile_boundary_is_enforced_on_the_call_not_just_the_listing() {
        let p = FakeExec::producer();
        for vetter_only in ["record_verdict", "unvetted", "pr_context", "pr_checkout"] {
            let resp = p.handle(&call(vetter_only, json!({"pr": "o/r#1", "verdict": "ready", "note": "n", "cost": 1, "basis": "b"}))).unwrap();
            assert!(is_error(&resp), "{vetter_only} must not exist for producer");
            assert!(text(&resp).contains("clone_create"));
        }
        assert!(
            p.calls().is_empty(),
            "no vetter transition reached an effect"
        );

        let v = FakeExec::ok();
        for producer_only in ["clone_create", "clone_gc", "clone_list"] {
            let resp = v
                .handle(&call(
                    producer_only,
                    json!({"repo": "o/r", "name": "x", "branch": "b"}),
                ))
                .unwrap();
            assert!(is_error(&resp), "{producer_only} must not exist for vetter");
        }
        assert!(v.calls().is_empty());
    }

    // Every tool the profiles LIST must be handled by the guard: a listed-but-unvalidated name would
    // be an advertised tool that always errors.
    #[test]
    fn every_listed_tool_is_handled_by_the_guard() {
        for profile in [McpProfile::Vetter, McpProfile::Producer] {
            for name in profile.tool_names() {
                let err = validate_call(profile, &["/work".to_string()], name, &json!({}))
                    .err()
                    .unwrap_or_default();
                assert!(
                    !err.contains("no such tool") && !err.contains("listed but not implemented"),
                    "{profile:?}/{name} is listed but unhandled: {err}"
                );
            }
        }
    }

    // --- the path guard: a refused clone argument never reaches an effect -------------------------

    #[test]
    fn a_clone_name_must_be_one_component_inside_the_root() {
        let root = "/home/gildlab/code";
        // the only accepted shapes: a bare name, or the full path of a direct child.
        assert_eq!(
            clone_name_in_root(root, "raindex-2444"),
            Ok("raindex-2444".to_string())
        );
        assert_eq!(
            clone_name_in_root(root, "/home/gildlab/code/raindex-2444"),
            Ok("raindex-2444".to_string())
        );
        assert_eq!(
            clone_name_in_root(root, "  /home/gildlab/code/vet-x-1/  "),
            Ok("vet-x-1".to_string())
        );
        // a trailing slash on the ROOT is the same root.
        assert_eq!(
            clone_name_in_root("/home/gildlab/code/", "x"),
            Ok("x".to_string())
        );

        for (bad, why) in [
            ("", "empty"),
            ("   ", "empty"),
            (".", "hidden/dot"),
            ("..", "traversal"),
            ("../etc", "traversal"),
            ("../../../etc/passwd", "traversal"),
            (
                "/home/gildlab/code/../../etc",
                "traversal laundered through the root prefix",
            ),
            (
                "/home/gildlab/code/x/../../y",
                "traversal after a valid component",
            ),
            ("/etc", "absolute, outside"),
            ("/etc/passwd", "absolute, outside"),
            ("/", "the filesystem root"),
            ("/home/gildlab", "an ANCESTOR of the root"),
            ("/home/gildlab/code", "the root itself"),
            ("/home/gildlab/code/", "the root itself"),
            // the sibling-prefix trick: `/home/gildlab/codeEVIL` shares a string prefix with the root
            // but is a different directory. This is the exact class of bug the deny rule had.
            (
                "/home/gildlab/codeEVIL/x",
                "sibling sharing a string prefix",
            ),
            ("/home/gildlab/code2", "sibling sharing a string prefix"),
            ("a/b", "nested"),
            ("raindex-2444/target", "nested"),
            ("//etc", "nested/absolute"),
            (".git", "dot-prefixed"),
            (".ssh", "dot-prefixed"),
            ("x\0y", "embedded NUL"),
        ] {
            assert!(
                clone_name_in_root(root, bad).is_err(),
                "{bad:?} must be refused ({why})"
            );
        }
        // a root that is not absolute is refused outright — it would resolve against an inherited cwd.
        assert!(clone_name_in_root("code", "x").is_err());
        assert!(clone_name_in_root("", "x").is_err());
        assert!(clone_name_in_root("./code", "x").is_err());
    }

    // The `..` check must be the one doing the work, not a coincidence of the later "direct child"
    // rule: a traversal has to be REPORTED as a traversal. Otherwise relaxing the direct-child rule
    // (e.g. to allow a nested clone dir) would silently re-open traversal.
    #[test]
    fn a_traversal_is_refused_as_a_traversal_not_incidentally() {
        let root = "/home/gildlab/code";
        for bad in [
            "..",
            "../etc",
            "/home/gildlab/code/../../etc",
            "/home/gildlab/code/x/../../y",
            "a/../../b",
        ] {
            let e = clone_name_in_root(root, bad).unwrap_err();
            assert!(
                e.contains("`..` traversal"),
                "{bad:?} must be refused FOR the traversal, got: {e}"
            );
        }
    }

    #[test]
    fn a_clone_resolves_against_any_configured_root_and_reports_them_all() {
        let roots = vec!["/work".to_string(), "/install".to_string()];
        assert_eq!(
            clone_in_roots(&roots, "raindex-1"),
            Ok(("/work".to_string(), "raindex-1".to_string()))
        );
        // the stranded vet-* clones: named by full path in the SECOND root.
        assert_eq!(
            clone_in_roots(&roots, "/install/vet-rain.flare-170"),
            Ok(("/install".to_string(), "vet-rain.flare-170".to_string()))
        );
        let err = clone_in_roots(&roots, "/etc/passwd").unwrap_err();
        assert!(err.contains("/work") && err.contains("/install"), "{err}");
        assert!(clone_in_roots(&[], "x").is_err());
    }

    // The refusal standard used for invalid verdicts, applied to the dangerous tool: an argument the
    // guard rejects must record ZERO calls — there is no effect for it to have partially performed.
    #[test]
    fn a_refused_clone_argument_reaches_no_effect() {
        let f = FakeExec::producer().with_roots(&["/work"]);
        for bad in [
            json!({"clone": "/etc"}),
            json!({"clone": "/"}),
            json!({"clone": ".."}),
            json!({"clone": "../../etc"}),
            json!({"clone": "/work/../../etc"}),
            json!({"clone": "/work"}),
            json!({"clone": "/workEVIL/x"}),
            json!({"clone": "sub/dir"}),
            json!({"clone": ".git"}),
            json!({"clone": ""}),
            json!({}),
            json!({"clone": 7}),
        ] {
            let resp = f.handle(&call("clone_release", bad.clone())).unwrap();
            assert!(is_error(&resp), "{bad} must be refused");
        }
        assert!(
            f.calls().is_empty(),
            "a refused clone argument performed no effect at all"
        );

        // …and the accepted shapes DO reach the effect, with the guard's output — not the raw string.
        f.handle(&call("clone_release", json!({"clone": "raindex-2444"})))
            .unwrap();
        f.handle(&call(
            "clone_release",
            json!({"clone": "/work/vet-x-1", "discard_uncommitted": true}),
        ))
        .unwrap();
        assert_eq!(
            f.calls(),
            vec![
                McpCall::CloneRelease {
                    root: "/work".to_string(),
                    name: "raindex-2444".to_string(),
                    discard_uncommitted: false,
                },
                McpCall::CloneRelease {
                    root: "/work".to_string(),
                    name: "vet-x-1".to_string(),
                    discard_uncommitted: true,
                },
            ]
        );
    }

    #[test]
    fn clone_create_validates_the_repo_the_name_and_the_branch() {
        let f = FakeExec::producer().with_roots(&["/work", "/install"]);
        for bad in [
            json!({"repo": "raindex", "name": "x", "branch": "b"}), // no owner
            json!({"repo": "a/b/c", "name": "x", "branch": "b"}),
            json!({"repo": "/b", "name": "x", "branch": "b"}),
            json!({"repo": "o/r", "name": "../x", "branch": "b"}),
            json!({"repo": "o/r", "name": "/etc", "branch": "b"}),
            json!({"repo": "o/r", "name": "a/b", "branch": "b"}),
            json!({"repo": "o/r", "name": "x"}),   // no branch
            json!({"repo": "o/r", "branch": "b"}), // no name
            json!({"name": "x", "branch": "b"}),   // no repo
            json!({"repo": "o/r", "name": "x", "branch": "--upload-pack=evil"}),
            json!({"repo": "o/r", "name": "x", "branch": "two words"}),
        ] {
            let resp = f.handle(&call("clone_create", bad.clone())).unwrap();
            assert!(is_error(&resp), "{bad} must be refused");
        }
        assert!(f.calls().is_empty());

        // a new clone is ALWAYS built in the first root, never in the legacy install root.
        f.handle(&call(
            "clone_create",
            json!({"repo": "rainlanguage/raindex", "name": "raindex-2444", "branch": "2026-07-22-issue-2444"}),
        ))
        .unwrap();
        assert_eq!(
            f.calls(),
            vec![McpCall::CloneCreate {
                root: "/work".to_string(),
                name: "raindex-2444".to_string(),
                slug: "rainlanguage/raindex".to_string(),
                branch: "2026-07-22-issue-2444".to_string(),
                base: None,
            }]
        );
    }

    #[test]
    fn clone_gc_bounds_its_age_cap() {
        let f = FakeExec::producer();
        for bad in [
            json!({"max_age_days": 0}), // would delete a clone the moment it exists
            json!({"max_age_days": -1}),
            json!({"max_age_days": 100000}),
            json!({"max_age_days": "30"}),
        ] {
            let resp = f.handle(&call("clone_gc", bad.clone())).unwrap();
            assert!(is_error(&resp), "{bad} must be refused");
        }
        assert!(f.calls().is_empty());
        f.handle(&call("clone_gc", json!({}))).unwrap();
        f.handle(&call(
            "clone_gc",
            json!({"dry_run": true, "max_age_days": 1}),
        ))
        .unwrap();
        assert_eq!(
            f.calls(),
            vec![
                McpCall::CloneGc {
                    max_age_days: GC_MAX_AGE_DEFAULT,
                    dry_run: false
                },
                McpCall::CloneGc {
                    max_age_days: 1,
                    dry_run: true
                },
            ]
        );
    }

    // --- the release decision --------------------------------------------------------------------

    fn st(unpushed: Option<u32>, dirt: Option<&str>) -> LocalCloneState {
        LocalCloneState {
            unpushed,
            dirt: dirt.map(String::from),
            branch: "b".to_string(),
        }
    }

    #[test]
    fn unpushed_work_refuses_release_and_no_flag_overrides_it() {
        for discard in [false, true] {
            assert!(release_decision(&st(Some(1), Some("")), discard).is_err());
            assert!(release_decision(&st(Some(9), Some("")), discard).is_err());
            // git could not answer -> treated as unpushed, the same fail-safe gc uses.
            assert!(release_decision(&st(None, Some("")), discard).is_err());
            // …and a clone whose STATUS is unknown is refused too.
            assert!(release_decision(&st(Some(0), None), discard).is_err());
        }
        let e = release_decision(&st(Some(3), Some("")), true).unwrap_err();
        assert!(
            e.contains("3 commit(s)") && e.contains("No flag overrides"),
            "{e}"
        );
    }

    #[test]
    fn uncommitted_changes_refuse_unless_the_caller_accepts_losing_them() {
        let dirty = st(Some(0), Some(" M Cargo.lock\n?? out/\n"));
        let e = release_decision(&dirty, false).unwrap_err();
        assert!(e.contains("2 uncommitted change(s)"), "{e}");
        assert!(e.contains("Cargo.lock"), "the refusal SHOWS the dirt: {e}");
        assert!(release_decision(&dirty, true).is_ok());
        // clean + pushed releases without any flag.
        assert!(release_decision(&st(Some(0), Some("")), false).is_ok());
    }

    // --- the filesystem half of the guard --------------------------------------------------------

    /// A disposable root with a real (empty but valid) git clone in it.
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("prr-clone-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn mk_clone(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join("README.md"), "x").unwrap();
        d
    }

    #[test]
    fn only_a_real_git_clone_directly_under_the_root_resolves() {
        let root = tmp_root("resolve");
        let rs = root.to_string_lossy().to_string();
        let good = mk_clone(&root, "raindex-1");
        assert_eq!(
            resolve_existing_clone(&rs, "raindex-1").unwrap(),
            std::fs::canonicalize(&good).unwrap()
        );

        // a plain directory with no .git is NOT a work clone — this is what keeps a malformed
        // argument away from ordinary data.
        std::fs::create_dir_all(root.join("not-a-clone/deep")).unwrap();
        std::fs::write(root.join("not-a-clone/precious.txt"), "keep me").unwrap();
        let e = resolve_existing_clone(&rs, "not-a-clone").unwrap_err();
        assert!(e.contains("no .git"), "{e}");

        // a FILE is refused; a missing entry is refused.
        std::fs::write(root.join("a-file"), "x").unwrap();
        assert!(resolve_existing_clone(&rs, "a-file").is_err());
        assert!(resolve_existing_clone(&rs, "nope").is_err());

        // a SYMLINK — even one pointing at a genuine clone — is refused: deleting it would act on
        // whatever it points at, which is the escape the guard exists to close.
        let escape = tmp_root("resolve-escape");
        let outside = mk_clone(&escape, "outside-clone");
        std::os::unix::fs::symlink(&outside, root.join("sneaky")).unwrap();
        let e = resolve_existing_clone(&rs, "sneaky").unwrap_err();
        assert!(e.contains("SYMLINK"), "{e}");
        // …and it is still there afterwards. A refusal has ZERO filesystem effect.
        assert!(outside.exists());
        assert!(root.join("not-a-clone/precious.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&escape);
    }

    // `resolve_existing_clone` is the SECOND layer, so it is tested on its own terms — called
    // directly with names `clone_name_in_root` would never emit. Reached only through the first
    // layer, its root/ancestor check is untestable, and an untested guard is not a guard.
    #[test]
    fn the_filesystem_guard_refuses_the_root_and_its_ancestors_on_its_own() {
        // Layout: <parent>/.git (so the ancestor LOOKS like a clone) and <parent>/root/<clone>.
        let parent = tmp_root("second-layer");
        std::fs::create_dir_all(parent.join(".git")).unwrap();
        std::fs::write(parent.join("irreplaceable.txt"), "everything").unwrap();
        let root = parent.join("root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let rs = root.to_string_lossy().to_string();
        mk_clone(&root, "legit");

        // The ancestor is a directory, is not a symlink, and has a `.git` — every OTHER check
        // passes. Only "must be a direct child of the root" stands between it and deletion.
        let e = resolve_existing_clone(&rs, "..").unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // The root itself, likewise.
        let e = resolve_existing_clone(&rs, ".").unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // …and the legitimate child still resolves, so the guard is not simply refusing everything.
        assert!(resolve_existing_clone(&rs, "legit").is_ok());

        assert!(parent.join("irreplaceable.txt").exists());
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn release_refuses_a_non_clone_without_touching_it() {
        let root = tmp_root("release-refuse");
        let rs = root.to_string_lossy().to_string();
        std::fs::create_dir_all(root.join("precious")).unwrap();
        std::fs::write(root.join("precious/data.txt"), "irreplaceable").unwrap();

        for name in ["precious", "..", "/etc", "nope"] {
            let (r, n) = match clone_in_roots(std::slice::from_ref(&rs), name) {
                Ok(v) => v,
                Err(_) => continue, // refused by the pure guard, before any path exists
            };
            assert!(clone_release_exec(&r, &n, true).is_err(), "{name}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join("precious/data.txt")).unwrap(),
            "irreplaceable",
            "a refused release left the directory byte-for-byte intact"
        );
        // The root itself is never removed, whatever is asked.
        assert!(root.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_real_release_removes_the_clone_and_reports_what_it_reclaimed() {
        let root = tmp_root("release-ok");
        let rs = root.to_string_lossy().to_string();
        // A real repo, so the git guards run against git rather than a stub.
        let d = root.join("throwaway");
        std::fs::create_dir_all(&d).unwrap();
        if git_run(&d, &["init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return; // no git in this sandbox
        }
        std::fs::write(d.join("f.txt"), vec![b'x'; 4096]).unwrap();

        // A brand-new repo has an UNBORN HEAD — no commits, so nothing can be lost — but untracked
        // files, so it is DIRTY: release refuses until the caller accepts losing them.
        let e = clone_release_exec(&rs, "throwaway", false).unwrap_err();
        assert!(e.contains("uncommitted"), "{e}");
        assert!(d.exists(), "the refusal did not delete anything");

        let out = clone_release_exec(&rs, "throwaway", true).unwrap();
        assert!(!d.exists(), "the clone is gone");
        assert!(root.exists(), "the root is not");
        assert!(out["bytes"].as_u64().unwrap() >= 4096, "{out}");
        assert_eq!(out["released"], json!("throwaway"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_committed_but_unpushed_clone_is_never_released() {
        let root = tmp_root("release-unpushed");
        let rs = root.to_string_lossy().to_string();
        let d = root.join("wip");
        std::fs::create_dir_all(&d).unwrap();
        if git_run(&d, &["init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let _ = git_run(&d, &["config", "user.email", "t@t"]);
        let _ = git_run(&d, &["config", "user.name", "t"]);
        std::fs::write(d.join("f.txt"), "work").unwrap();
        git_run(&d, &["add", "-A"]).unwrap();
        git_run(&d, &["-c", "commit.gpgsign=false", "commit", "-qm", "wip"]).unwrap();

        // clean tree, one commit on no remote: refused even with the discard flag set.
        for discard in [false, true] {
            let e = clone_release_exec(&rs, "wip", discard).unwrap_err();
            assert!(e.contains("exist only in this clone"), "{e}");
        }
        assert!(d.join("f.txt").exists(), "the commit is still on disk");
        let _ = std::fs::remove_dir_all(&root);
    }

    // An interrupted clone (a `.git` with no commit yet) is NOT an unknown push state — there is
    // nothing in it to lose. Reading it as unknown made every half-finished clone immortal, since
    // both the sweep and release fail safe on `None`.
    #[test]
    fn an_unborn_head_reads_as_zero_unpushed_not_as_unknown() {
        let root = tmp_root("unborn");
        let d = root.join("half-cloned");
        std::fs::create_dir_all(&d).unwrap();
        if git_run(&d, &["init", "-q"]).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert_eq!(local_clone_state(&d).unpushed, Some(0));
        // …while a directory that is not a repo at all stays genuinely unknown.
        let notrepo = root.join("not-a-repo");
        std::fs::create_dir_all(&notrepo).unwrap();
        assert_eq!(local_clone_state(&notrepo).unpushed, None);
        assert!(release_decision(&local_clone_state(&notrepo), true).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- the sweep --------------------------------------------------------------------------------

    #[test]
    fn the_sweep_only_considers_git_clones_directly_under_a_root() {
        let root = tmp_root("sweep");
        let rs = root.to_string_lossy().to_string();
        mk_clone(&root, "a-clone");
        std::fs::create_dir_all(root.join("plain-dir/nested")).unwrap();
        std::fs::write(root.join("loose-file"), "x").unwrap();
        // a clone one level too deep is NOT a candidate.
        mk_clone(&root.join("plain-dir"), "deep-clone");

        let mut seen = Vec::new();
        let recs = gc_clones_sweep(&rs, 30, true, &mut |r| seen.push(r.name.clone())).unwrap();
        assert_eq!(
            recs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["a-clone"]
        );
        assert_eq!(
            seen,
            vec!["a-clone"],
            "every decision is streamed as it is made"
        );
        // dry-run touches nothing.
        assert!(root.join("a-clone").exists());
        assert!(root.join("plain-dir/nested").exists());
        assert!(root.join("loose-file").exists());
        // an unreadable root is an error, not a silent zero-clone success.
        assert!(gc_clones_sweep("/no/such/root", 30, true, &mut |_| {}).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_size_counts_files_and_never_follows_symlinks() {
        let root = tmp_root("size");
        std::fs::create_dir_all(root.join("d/sub")).unwrap();
        std::fs::write(root.join("d/a"), vec![b'x'; 1000]).unwrap();
        std::fs::write(root.join("d/sub/b"), vec![b'x'; 2000]).unwrap();
        assert_eq!(dir_size_bytes(&root.join("d")), 3000);
        // a symlink to a big tree outside must not be counted as if it lived inside.
        std::fs::write(root.join("huge"), vec![b'x'; 100_000]).unwrap();
        std::os::unix::fs::symlink(root.join("huge"), root.join("d/link")).unwrap();
        let with_link = dir_size_bytes(&root.join("d"));
        assert!(
            with_link < 4000,
            "a symlink must not pull its target's size in: {with_link}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn human_bytes_is_readable_at_every_scale() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(195 * 1024 * 1024 * 1024), "195.0 GB");
    }

    // #81: a `vet-*` checkout is reclaimed by the sweep even though its PR is open, end to end
    // through the real filesystem — `gc_reclaims_a_stale_vet_checkout_whose_pr_is_still_open` pins
    // the decision, this pins that the sweep applies it to a directory `pr_checkout` would create.
    // The clone here has NO remote, so `resolve_pr_state` cannot answer and the OLD code fell to the
    // 30-day no-PR backstop; against real GitHub it answered "open PR" and kept it forever. Either
    // way the directory survived, and either way it must not now.
    #[test]
    fn the_sweep_reclaims_a_leaked_vet_checkout_but_not_a_producer_clone() {
        let outer = tmp_root("sweep-vet");
        // The upstream lives OUTSIDE the swept root, so the sweep sees only the two clones.
        let up = outer.join("upstream");
        let root = outer.join("root");
        std::fs::create_dir_all(&up).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let rs = root.to_string_lossy().to_string();
        if git_run(&up, &["init", "-q", "-b", "main"]).is_err() {
            let _ = std::fs::remove_dir_all(&outer);
            panic!("git is required to run this test");
        }
        let id = ["-c", "user.email=t@t", "-c", "user.name=t"];
        std::fs::write(up.join("src.sol"), "contract C {}").unwrap();
        git_run(&up, &["add", "-A"]).unwrap();
        let mut c = id.to_vec();
        c.extend_from_slice(&["-c", "commit.gpgsign=false", "commit", "-qm", "the PR head"]);
        git_run(&up, &c).unwrap();

        // A leaked audit checkout: a clean clone whose commit is on origin — exactly what
        // `pr_checkout` leaves behind when the run dies before `clone_release`.
        let leaked = root.join("vet-rain.factory-47");
        git_run(
            std::path::Path::new("."),
            &[
                "clone",
                "-q",
                &format!("file://{}", up.display()),
                &leaked.to_string_lossy(),
            ],
        )
        .unwrap();
        // Age it past the vet cap. `clone_age_days` reads the NEWER of the directory and
        // `.git/HEAD`, so a leaked clone is only idle when nothing has checked out in it either.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        let age = |d: &std::path::Path| {
            filetime_set(d, old);
            filetime_set(&d.join(".git/HEAD"), old);
        };
        age(&leaked);

        // A producer work clone of the same age, holding one commit that exists only here.
        let work = root.join("rain.factory-46");
        git_run(
            std::path::Path::new("."),
            &[
                "clone",
                "-q",
                &format!("file://{}", up.display()),
                &work.to_string_lossy(),
            ],
        )
        .unwrap();
        std::fs::write(work.join("fix.sol"), "contract Fix {}").unwrap();
        git_run(&work, &["add", "-A"]).unwrap();
        let mut c = id.to_vec();
        c.extend_from_slice(&["-c", "commit.gpgsign=false", "commit", "-qm", "wip"]);
        git_run(&work, &c).unwrap();
        age(&work);

        let recs = gc_clones_sweep(&rs, 30, false, &mut |_| {}).unwrap();
        let outcome = |n: &str| {
            recs.iter()
                .find(|r| r.name == n)
                .map(|r| (r.outcome, r.reason.clone()))
                .unwrap_or(("missing", String::new()))
        };
        assert_eq!(
            outcome("vet-rain.factory-47"),
            ("deleted", "vet checkout, idle 3d".to_string())
        );
        assert!(!leaked.exists(), "the leaked audit checkout is gone");
        // The producer clone holds a commit on no remote — unpushed work, kept whatever its age.
        assert_eq!(
            outcome("rain.factory-46"),
            ("kept", "1 unpushed commit(s)".to_string())
        );
        assert!(work.exists(), "a clone holding work is never touched");
        let _ = std::fs::remove_dir_all(&outer);
    }

    // The race the vet cap opens if "idle" is read off the directory's mtime alone. A checkout
    // rewrites files BELOW the top level, so it does not touch the directory's own mtime — a clone
    // the vetter checked out minutes ago can read as days idle, and the midnight sweep would delete
    // the working tree of a run still using it (a 23:00 run reusing yesterday's checkout is exactly
    // the case). `.git/HEAD` is rewritten by every checkout, including the no-op `-f -B` onto the
    // current branch, and is not touched by the `git status` this sweep itself runs.
    #[test]
    fn a_clone_checked_out_recently_is_not_idle_however_old_its_directory_looks() {
        let root = tmp_root("age");
        let d = root.join("clone");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(9 * 86_400);
        std::fs::write(d.join(".git/HEAD"), "ref: refs/heads/pr-47\n").unwrap();
        filetime_set(&d, ancient);
        filetime_set(&d.join(".git/HEAD"), ancient);
        assert_eq!(
            clone_age_days(&d),
            9,
            "nothing has happened here for 9 days"
        );

        // …now check it out again: only `.git/HEAD` moves, and the clone is live.
        filetime_set(&d.join(".git/HEAD"), std::time::SystemTime::now());
        assert_eq!(
            clone_age_days(&d),
            0,
            "a clone checked out just now is not idle, whatever its directory mtime says"
        );
        // A clone with no `.git/HEAD` at all still ages off its directory.
        std::fs::remove_file(d.join(".git/HEAD")).unwrap();
        assert_eq!(clone_age_days(&d), 9);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Backdate a file's or directory's mtime, via `std::fs::File::set_times` rather than a `touch`
    /// subprocess. The subprocess form needs GNU `touch` for `-d @<epoch>`; BSD `touch` rejects it,
    /// so on macOS it works only because `rainix-rs-test` runs inside a nix shell that puts nixpkgs
    /// coreutils on PATH. A test whose result depends on which `touch` is ahead on PATH is a test
    /// with a hidden precondition; this has none.
    fn filetime_set(p: &std::path::Path, t: std::time::SystemTime) {
        let f = std::fs::File::options()
            .read(true)
            .open(p)
            .unwrap_or_else(|e| panic!("open {}: {e}", p.display()));
        f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
            .unwrap_or_else(|e| panic!("set_times {}: {e}", p.display()));
    }
}

// ─── pr_checkout: the audit lens's working tree ──────────────────────────────
//
// These drive REAL `git` against a REAL local repository over `file://`, because the bug in #81 is a
// property of git's refspec handling in a shallow clone — a stub would have asserted our belief
// about git rather than what git does. Nothing here touches the network: the "upstream" is a
// directory, and `refs/pull/<n>/head` is written into it with `update-ref` exactly as GitHub
// publishes it.
#[cfg(test)]
mod pr_checkout_tests {
    use super::{
        checkout_branch, checkout_dir, checkout_failure_error, checkout_pr_head, git_out, git_run,
        local_clone_state, pr_checkout_at, pr_head_refspec,
    };

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("prr-checkout-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn commit(dir: &std::path::Path, msg: &str) -> String {
        git_run(dir, &["add", "-A"]).unwrap();
        git_run(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                msg,
            ],
        )
        .unwrap();
        git_out(dir, &["rev-parse", "HEAD"]).unwrap()
    }

    /// An "upstream" whose PR head is published ONLY at `refs/pull/<n>/head` — the shape a FORK PR
    /// has, and the shape a same-repo PR has as far as a single-branch shallow clone can see.
    /// Returns (upstream path, PR head sha).
    fn upstream(root: &std::path::Path, num: u64) -> (std::path::PathBuf, String) {
        let up = root.join("upstream");
        std::fs::create_dir_all(&up).unwrap();
        git_run(&up, &["init", "-q", "-b", "main"]).expect("git is required to run this test");
        std::fs::write(up.join("base.txt"), "base").unwrap();
        let base = commit(&up, "base");
        git_run(&up, &["checkout", "-q", "-b", "pr-head"]).unwrap();
        std::fs::write(up.join("only-in-the-pr.sol"), "contract Reviewed {}").unwrap();
        let head = commit(&up, "the PR commit");
        git_run(&up, &["checkout", "-q", "main"]).unwrap();
        git_run(
            &up,
            &["update-ref", &format!("refs/pull/{num}/head"), &head],
        )
        .unwrap();
        assert_ne!(base, head);
        (up, head)
    }

    /// The clone `pr_checkout` makes: `--depth 1`, which is what leaves
    /// `remote.origin.fetch = +refs/heads/main:refs/remotes/origin/main`.
    fn shallow_clone(up: &std::path::Path, dest: &std::path::Path) {
        git_run(
            std::path::Path::new("."),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &format!("file://{}", up.display()),
                &dest.to_string_lossy(),
            ],
        )
        .unwrap();
        assert_eq!(
            git_out(dest, &["config", "--get", "remote.origin.fetch"]).unwrap(),
            "+refs/heads/main:refs/remotes/origin/main",
            "the fixture must reproduce the SINGLE-BRANCH refspec a shallow clone gets — that is the \
             whole precondition of #81"
        );
    }

    // THE regression. On a shallow clone the old path (`gh pr checkout`, i.e. fetch the head with no
    // destination refspec then `git checkout --track origin/<head>`) died with "cannot set up
    // tracking information; starting point 'origin/<head>' is not a branch" — for EVERY same-repo
    // PR, so the audit lens never ran at all.
    #[test]
    fn a_shallow_clone_gets_the_pr_head_checked_out() {
        let root = tmp_root("head");
        let (up, head) = upstream(&root, 47);
        let clone = root.join("vet-upstream-47");
        shallow_clone(&up, &clone);

        assert_eq!(checkout_pr_head(&clone, 47).unwrap(), head);
        assert_eq!(git_out(&clone, &["rev-parse", "HEAD"]).unwrap(), head);
        assert_eq!(
            git_out(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            checkout_branch(47)
        );
        // The audit lens reads FILES, so the working tree — not just the ref — must be the PR's.
        assert_eq!(
            std::fs::read_to_string(clone.join("only-in-the-pr.sol")).unwrap(),
            "contract Reviewed {}"
        );
        assert_eq!(
            git_out(&clone, &["status", "--porcelain"]).unwrap(),
            "",
            "a checkout that leaves dirt would make the clone unreleasable"
        );
        // Still shallow: the fix must not have traded #81 for the disk-full outage.
        assert_eq!(
            git_out(&clone, &["rev-parse", "--is-shallow-repository"]).unwrap(),
            "true"
        );
        assert_eq!(
            git_out(&clone, &["rev-list", "--count", "HEAD"]).unwrap(),
            "1"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The second, quieter half of the leak. `gh pr checkout` puts a FORK PR's head on a plain LOCAL
    // branch, so `rev-list HEAD --not --remotes` counts the whole branch as unpushed and BOTH
    // `clone_release` and the sweep refuse the clone forever. Fetching into `refs/remotes/origin/pr/<n>`
    // is what makes the checked-out commit provably pushed — and therefore reclaimable.
    #[test]
    fn the_checked_out_head_counts_as_pushed_so_the_clone_stays_reclaimable() {
        let root = tmp_root("pushed");
        let (up, _head) = upstream(&root, 12);
        let clone = root.join("vet-upstream-12");
        shallow_clone(&up, &clone);
        checkout_pr_head(&clone, 12).unwrap();

        let s = local_clone_state(&clone);
        assert_eq!(
            s.unpushed,
            Some(0),
            "the PR head must be reachable from a remote-tracking ref, or the clone is immortal"
        );
        assert_eq!(s.dirt.as_deref(), Some(""));
        assert!(super::release_decision(&s, false).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    // The refspec is load-bearing in two independent ways; both are asserted rather than implied.
    #[test]
    fn the_refspec_names_the_pull_ref_and_an_explicit_remote_destination() {
        let r = pr_head_refspec(47);
        assert_eq!(r, "+refs/pull/47/head:refs/remotes/origin/pr/47");
        assert!(
            r.contains("refs/pull/"),
            "a heads-based refspec cannot see a fork PR's head at all"
        );
        assert!(
            r.split(':').nth(1).unwrap().starts_with("refs/remotes/"),
            "no destination (or a non-remote one) is what makes the head read as unpushed"
        );
    }

    // THE wrong-tree defect. `gh repo clone --depth 1` SUCCEEDED and only the checkout failed, so the
    // old code returned an error while leaving `vet-<repo>-<n>` on disk holding the DEFAULT BRANCH —
    // a directory named after this PR, at the exact path the audit lens looks for, containing code
    // the PR never touched. (The live artifact: /home/gildlab/code/vet-rain.factory-47, HEAD 832e457
    // = main, while rain.factory#47's head is 58a938e.) The postcondition is now binary: the PR head,
    // or nothing.
    #[test]
    fn a_failed_checkout_leaves_no_directory_at_the_path_the_audit_lens_would_read() {
        let root = tmp_root("teardown");
        let (up, _) = upstream(&root, 47);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // The clone exists and is valid — as it does after `gh repo clone` succeeds. Only the PR ref
        // is missing (PR 99 was never opened), so the fetch fails exactly where the old one did.
        let dir = checkout_dir(&work.to_string_lossy(), "o/upstream", 99);
        let path = std::path::PathBuf::from(&dir);
        shallow_clone(&up, &path);
        assert!(
            path.join("base.txt").exists(),
            "the wrong tree IS on disk here"
        );

        let e = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 99).unwrap_err();
        assert!(
            !path.exists(),
            "the failed checkout must not leave a tree at {dir}"
        );
        // …and the error is the one that tells the caller not to go looking for a replacement.
        assert!(e.contains("There is NO checkout"), "{e}");
        assert!(e.contains("record NO verdict"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The chain #81 observed ran: checkout fails → the vetter searches → a `vet-*` leftover from an
    // unrelated run answers the search → the audit lens enumerates the WRONG repo's sources. Two
    // links are closed here. The tool never names a directory it did not create (so nothing in its
    // output can be mistaken for one), and its failure message names the exact wrong answer a
    // filesystem search returns. The third link — that the leftover exists at all — is
    // `the_sweep_reclaims_a_leaked_vet_checkout_but_not_a_producer_clone`; the fourth is the vetter
    // prompt's "never search for a checkout".
    #[test]
    fn a_leftover_vet_directory_is_never_offered_as_a_substitute_tree() {
        let root = tmp_root("leftover");
        let (up, _) = upstream(&root, 47);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // Exactly the observed leftover: another run's checkout, full of plausible sources.
        let leftover = work.join("vet-rain.factory-dep");
        std::fs::create_dir_all(leftover.join("src")).unwrap();
        std::fs::write(
            leftover.join("src/LibCloneFactoryDeploy.sol"),
            "// other PR",
        )
        .unwrap();

        let dir = checkout_dir(&work.to_string_lossy(), "o/upstream", 99);
        shallow_clone(&up, std::path::Path::new(&dir));
        let e = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 99).unwrap_err();

        assert!(
            !e.contains("vet-rain.factory-dep"),
            "the tool must not hand back a path it did not create: {e}"
        );
        assert!(
            e.contains("Do NOT search the filesystem"),
            "the refusal forbids the move that produced the wrong-tree verdict: {e}"
        );
        assert!(
            e.contains("some OTHER PR's checkout"),
            "…and names what a search would actually return: {e}"
        );
        // The leftover is another run's business: refusing must not delete it either.
        assert!(leftover.join("src/LibCloneFactoryDeploy.sol").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // A reused checkout must end at the PR head whatever state the last run left it in — the tool's
    // contract is the tree, not a best effort at it. Without `-f`, a modified file makes `checkout`
    // refuse, which under the postcondition above would DELETE a clone that only needed resetting.
    #[test]
    fn a_reused_checkout_is_reset_to_the_pr_head_even_when_the_tree_was_modified() {
        let root = tmp_root("reuse");
        let (up, head) = upstream(&root, 47);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let dir = checkout_dir(&work.to_string_lossy(), "o/upstream", 47);
        let path = std::path::PathBuf::from(&dir);
        shallow_clone(&up, &path);

        let first = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 47).unwrap();
        assert_eq!(first["reused"], serde_json::json!(true));
        assert_eq!(first["head"], serde_json::json!(head));

        std::fs::write(path.join("only-in-the-pr.sol"), "contract Tampered {}").unwrap();
        let again = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 47).unwrap();
        assert_eq!(again["head"], serde_json::json!(head));
        assert_eq!(
            std::fs::read_to_string(path.join("only-in-the-pr.sol")).unwrap(),
            "contract Reviewed {}",
            "the reused clone was reset to the PR head, not left half-modified"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The vetter burned four Glob calls — two of them 20-second ripgrep timeouts against
    // /home/gildlab/code — looking for a directory the tool already knew. The success value carries
    // the path AND the sha, so "locate my own tool's output" is not a step that exists.
    #[test]
    fn a_successful_checkout_returns_the_path_and_the_sha_it_produced() {
        let root = tmp_root("returns");
        let (up, head) = upstream(&root, 47);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let dir = checkout_dir(&work.to_string_lossy(), "o/upstream", 47);
        shallow_clone(&up, std::path::Path::new(&dir));

        let out = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 47).unwrap();
        assert_eq!(out["dir"], serde_json::json!(dir));
        assert_eq!(out["head"], serde_json::json!(head));
        assert_eq!(out["branch"], serde_json::json!(checkout_branch(47)));
        assert_eq!(out["pr"], serde_json::json!("o/upstream#47"));
        // The sha is what lets the caller cross-check the tree against `pr_context.headRefOid`
        // instead of trusting that the right thing happened.
        assert_eq!(
            git_out(std::path::Path::new(&dir), &["rev-parse", "HEAD"]).unwrap(),
            out["head"].as_str().unwrap()
        );
        // The note tells the reader the same thing the prompt does, so the rule survives a prompt
        // edit that forgets it.
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("ONLY tree"), "{note}");
        assert!(note.contains("clone_release"), "{note}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The one failure that must NOT delete: something that is not our clone sits at the path. The
    // teardown exists to remove a tree WE made; turning it into "delete whatever is in the way"
    // would be a far worse bug than the one it fixes.
    #[test]
    fn a_non_clone_at_the_checkout_path_is_refused_without_being_touched() {
        let root = tmp_root("occupied");
        let work = root.join("work");
        let dir = checkout_dir(&work.to_string_lossy(), "o/upstream", 47);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/irreplaceable.txt"), "everything").unwrap();

        let e = pr_checkout_at(&work.to_string_lossy(), "o/upstream", 47).unwrap_err();
        assert!(e.contains("not a git clone"), "{e}");
        assert!(e.contains("nothing was changed"), "{e}");
        assert_eq!(
            std::fs::read_to_string(format!("{dir}/irreplaceable.txt")).unwrap(),
            "everything"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The message is the whole of link 2→3, so its content is pinned rather than left to prose drift.
    #[test]
    fn the_failure_message_states_the_postcondition_and_the_only_two_next_moves() {
        let m = checkout_failure_error("o/r#47", "/work/vet-r-47", "git fetch failed: no such ref");
        assert!(m.starts_with("error:"), "{m}");
        assert!(m.contains("o/r#47") && m.contains("/work/vet-r-47"), "{m}");
        assert!(
            m.contains("git fetch failed: no such ref"),
            "the cause survives: {m}"
        );
        assert!(m.contains("does not exist"), "{m}");
        assert!(m.contains("Re-call `pr_checkout` ONCE"), "{m}");
        assert!(m.contains("record NO verdict"), "{m}");
    }
}

// The three CI closure gates (#85), as behaviour rather than as YAML.
//
// These were inline bash in `.github/workflows/rust.yml` — a PDF written in `printf`, a PATH
// scraped with `sed`, a set difference computed with `comm` — which meant the gates that decide
// whether the pipeline can READ ITS EVIDENCE were the one part of the repo `cargo test` could not
// see and nobody could run locally. Everything below is the same three decisions, made where they
// can be tested and mutated.
//
// The render gate is driven through STUB renderers rather than real poppler: the assertions here
// are about what the gate concludes from what a renderer did (exit non-zero, write nothing, write
// something that is not an image, see an environment it should not), and a real pdftoppm can only
// ever demonstrate the success path. Real poppler parsing the real fixture is what
// `closure-render` itself does in CI.
#[cfg(test)]
mod closure_gate_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch directory that removes itself, named per test so cargo's threads cannot collide.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("prr-closure-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch dir");
            Scratch(p)
        }
        fn join(&self, n: &str) -> std::path::PathBuf {
            self.0.join(n)
        }
        /// A directory of executables, standing in for one store path on a runner's PATH.
        fn bindir(&self, n: &str) -> std::path::PathBuf {
            let d = self.0.join(n);
            std::fs::create_dir_all(&d).expect("bin dir");
            d
        }

        /// Write an executable stub, and do not return until the kernel will actually exec it.
        ///
        /// `cargo test` is multi-threaded and every spawn snapshots the process's open file
        /// descriptors, so another test's `Command` can be holding this file's write handle at the
        /// moment this one tries to run it — ETXTBSY, surfacing as `Unspawnable(ExecutableFileBusy)`
        /// in place of whatever the test was asserting. That is a property of writing an executable
        /// inside a threaded process, not of the gate under test, so it is waited out here rather
        /// than left to flake an assertion. Errno, not a message, decides.
        fn stub(&self, dir: &std::path::Path, name: &str, body: &str) {
            let p = dir.join(name);
            std::fs::write(&p, body).expect("stub");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            // The probe RUNS the stub, so its argv target is a directory nothing reads back — a
            // page written into the render dir would be counted as output.
            let probe = self.0.join(".probe");
            std::fs::create_dir_all(&probe).expect("probe dir");
            for _ in 0..500 {
                match Command::new(&p)
                    .arg(probe.join("probe"))
                    .env_clear()
                    .output()
                {
                    Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    _ => return,
                }
            }
            panic!("{} never became executable", p.display());
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn closure_of(dir: &std::path::Path) -> RunnerClosure {
        RunnerClosure {
            pkg: "campaign-run".to_string(),
            path: std::ffi::OsString::from(dir.to_string_lossy().to_string()),
        }
    }

    fn surface(pkg: &str, bins: &[&str]) -> (String, BTreeSet<String>) {
        (
            pkg.to_string(),
            bins.iter().map(|b| b.to_string()).collect(),
        )
    }

    // ---- the declaration itself ----

    #[test]
    fn the_model_runners_are_the_two_that_host_a_harness() {
        // Both, not just the vetter. The producer drains the `audit`-labelled backlog first and
        // those issues cite the PDF as their source, so a producer that cannot open the report it
        // implements against writes a PR body asserting something it never read.
        assert!(MODEL_RUNNERS.contains(&"campaign-run"), "{MODEL_RUNNERS:?}");
        assert!(MODEL_RUNNERS.contains(&"review-run"), "{MODEL_RUNNERS:?}");
    }

    #[test]
    fn every_declared_asymmetry_names_a_model_runner_and_carries_a_reason() {
        for d in DECLARED_ASYMMETRY {
            assert!(
                MODEL_RUNNERS.contains(&d.only_in),
                "{} is declared only-in {}, which is not a model runner",
                d.bin,
                d.only_in
            );
            assert!(
                !d.why.trim().is_empty(),
                "{} must say why it is one-sided",
                d.bin
            );
        }
    }

    // ---- baked_path: the gate's only window onto a closure ----

    #[test]
    fn baked_path_lifts_the_first_export_path_from_a_runner_script() {
        let script = "#!/nix/store/x/bin/bash\nset -o errexit\nexport PATH=\"/nix/store/a/bin:/nix/store/b/bin\"\nexport PATH=\"/decoy\"\n";
        assert_eq!(
            baked_path(script),
            Some("/nix/store/a/bin:/nix/store/b/bin"),
            "writeShellApplication bakes the runtimeInputs PATH first; a later line is the script's own"
        );
    }

    #[test]
    fn a_script_with_no_usable_baked_path_is_unevaluable_not_empty() {
        // Each of these would otherwise resolve to "no tools missing" over an empty environment —
        // a gate that passes hardest when it can see nothing at all.
        assert_eq!(baked_path("#!/bin/sh\necho hi\n"), None, "no export at all");
        assert_eq!(baked_path("export PATH=\"\"\n"), None, "empty PATH");
        assert_eq!(
            baked_path("export PATH=\"/nix/store/a\n"),
            None,
            "unterminated"
        );
    }

    // ---- exit codes: knowing nothing is not the same as knowing it is wrong ----

    #[test]
    fn an_unevaluable_gate_outranks_an_unsatisfied_one() {
        let missing = ClosureFault::ToolMissing {
            pkg: "review-run".to_string(),
            bin: "pdftoppm",
            why: "why",
        };
        let unbuilt = ClosureFault::Unbuildable {
            pkg: "review-run".to_string(),
            detail: "error: flake output not found".to_string(),
        };
        assert_eq!(closure_exit_code(&[]), 0);
        assert_eq!(closure_exit_code(std::slice::from_ref(&missing)), 12);
        assert_eq!(closure_exit_code(std::slice::from_ref(&unbuilt)), 2);
        assert_eq!(
            closure_exit_code(&[missing, unbuilt]),
            2,
            "a broken build sends the reader to flake.nix, not to a missing tool"
        );
        assert_eq!(
            closure_exit_code(&[ClosureFault::NoBakedPath {
                pkg: "review-run".to_string(),
                script: "/nix/store/x/bin/review-run".to_string(),
            }]),
            2
        );
    }

    // ---- preflight, over a PATH that is a parameter ----

    #[test]
    fn preflight_answers_for_the_path_it_is_given_not_the_process_environment() {
        let s = Scratch::new("preflight");
        let bin = s.bindir("bin");
        for t in HARNESS_TOOLS {
            s.stub(&bin, t.bin, "#!/bin/sh\nexit 0\n");
        }
        let path = std::ffi::OsString::from(bin.to_string_lossy().to_string());
        assert_eq!(
            preflight_report(&path),
            0,
            "a PATH carrying every declared tool satisfies preflight"
        );
        // Drop one and the same code says so — this is the exact question `closure-preflight`
        // asks of each runner's baked PATH, so the two cannot answer differently.
        std::fs::remove_file(bin.join(HARNESS_TOOLS[0].bin)).unwrap();
        assert_eq!(preflight_report(&path), CLOSURE_UNSATISFIED);
        assert_eq!(
            preflight_report(&std::ffi::OsString::new()),
            CLOSURE_UNSATISFIED,
            "an empty environment resolves nothing"
        );
    }

    // ---- the fixture: generated, and self-checking ----

    #[test]
    fn the_generated_pdf_is_a_real_document() {
        let pdf = fixture_pdf();
        let text = String::from_utf8(pdf.clone()).expect("the fixture is ASCII by construction");
        assert!(text.starts_with("%PDF-1.4\n"), "a PDF header, first");
        assert!(text.ends_with("%%EOF\n"), "and an EOF marker, last");
        // The object graph a renderer has to walk to produce a page.
        for required in [
            "/Type /Catalog",
            "/Type /Pages",
            "/Type /Page",
            "/MediaBox [0 0 612 792]",
            "/Type /Font",
            "Tj ET",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
        // The content stream declares its own length, and a wrong one is how a hand-built PDF
        // usually dies.
        let stream = text
            .split_once("stream\n")
            .and_then(|(_, r)| r.split_once("endstream"))
            .map(|(s, _)| s)
            .expect("a content stream");
        assert!(
            text.contains(&format!("/Length {}", stream.len())),
            "declared /Length must equal the {} bytes actually written",
            stream.len()
        );
    }

    #[test]
    fn every_xref_offset_points_at_the_object_it_claims() {
        // The self-check the generated fixture is FOR: an xref entry is a byte offset, and a
        // generator that computes one wrong produces a file poppler rejects. Read the table back
        // and follow it, exactly as a parser would — an oracle independent of how it was built.
        let pdf = fixture_pdf();
        let text = String::from_utf8(pdf.clone()).unwrap();

        let startxref: usize = text
            .rsplit_once("startxref\n")
            .and_then(|(_, r)| r.split_once('\n'))
            .and_then(|(n, _)| n.trim().parse().ok())
            .expect("a startxref");
        assert!(
            text[startxref..].starts_with("xref\n"),
            "startxref must point AT the xref table"
        );

        let table = &text[startxref..];
        let mut lines = table.lines();
        assert_eq!(lines.next(), Some("xref"));
        let header = lines.next().expect("a subsection header");
        let count: usize = header.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert!(
            header.starts_with("0 "),
            "the subsection starts at object 0"
        );

        let free = lines.next().unwrap();
        assert_eq!(free, "0000000000 65535 f ", "entry 0 heads the free list");

        let mut objects = 0;
        for (i, entry) in lines.take(count - 1).enumerate() {
            let off: usize = entry.split_whitespace().next().unwrap().parse().unwrap();
            assert!(
                text[off..].starts_with(&format!("{} 0 obj", i + 1)),
                "xref entry {} points at {:?}, not at object {}",
                i + 1,
                &text[off..off + 12.min(text.len() - off)],
                i + 1
            );
            objects += 1;
        }
        assert_eq!(
            objects,
            count - 1,
            "every entry but the free head is an object"
        );
        assert!(
            text.contains(&format!("/Size {count}")),
            "the trailer's /Size must match the table's entry count"
        );
    }

    // ---- the render gate: what it concludes from what a renderer did ----

    #[test]
    fn a_closure_without_pdftoppm_cannot_render() {
        let s = Scratch::new("render-absent");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        assert_eq!(
            render_from_closure(&closure_of(&bin), &pdf, &dir),
            Err(RenderFault::NotInClosure)
        );
    }

    #[test]
    fn a_renderer_that_exits_nonzero_is_a_render_failure_not_an_absence() {
        // The case `command -v` structurally cannot see: the binary IS there, and a missing
        // shared library or data file makes it fail anyway. The harness turns this into the same
        // `isError` an absent binary produces, and the run still exits 0.
        let s = Scratch::new("render-exit");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(
            &bin,
            "pdftoppm",
            "#!/bin/sh\necho 'error while loading shared libraries' >&2\nexit 127\n",
        );
        assert_eq!(
            render_from_closure(&closure_of(&bin), &pdf, &dir),
            Err(RenderFault::Exited {
                code: Some(127),
                stderr: "error while loading shared libraries".to_string(),
            })
        );
    }

    #[test]
    fn a_renderer_that_writes_no_pages_fails_even_on_exit_zero() {
        // The harness counts the files pdftoppm left behind, so zero files is its "cannot read
        // this document" path — and exit 0 is not evidence of anything on its own.
        let s = Scratch::new("render-nopages");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(&bin, "pdftoppm", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            render_from_closure(&closure_of(&bin), &pdf, &dir),
            Err(RenderFault::NoPages)
        );
    }

    #[test]
    fn a_page_that_is_not_a_jpeg_fails_the_render_gate() {
        let s = Scratch::new("render-notjpeg");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(
            &bin,
            "pdftoppm",
            "#!/bin/sh\nfor a in \"$@\"; do out=\"$a\"; done\nprintf 'not an image' > \"$out-01.jpg\"\n",
        );
        match render_from_closure(&closure_of(&bin), &pdf, &dir) {
            Err(RenderFault::NotJpeg { magic, .. }) => assert_eq!(magic, b"not".to_vec()),
            other => panic!("expected NotJpeg, got {other:?}"),
        }
    }

    #[test]
    fn a_render_that_produces_a_jpeg_passes_and_counts_its_pages() {
        let s = Scratch::new("render-ok");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(
            &bin,
            "pdftoppm",
            "#!/bin/sh\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '\\377\\330\\377\\340jpeg' > \"$out-01.jpg\"\nprintf '\\377\\330\\377\\340jpeg' > \"$out-02.jpg\"\n",
        );
        assert_eq!(render_from_closure(&closure_of(&bin), &pdf, &dir), Ok(2));
    }

    #[test]
    fn the_render_gate_uses_the_harness_argv() {
        // Byte-for-byte what Claude Code's Read tool spawns. Approximating it would exercise a
        // call the pipeline never makes, and the flags are where a broken poppler shows up.
        let s = Scratch::new("render-argv");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(
            &bin,
            "pdftoppm",
            "#!/bin/sh\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%s\\n' \"$@\" > \"$out.argv\"\nprintf '\\377\\330\\377' > \"$out-01.jpg\"\n",
        );
        assert_eq!(render_from_closure(&closure_of(&bin), &pdf, &dir), Ok(1));
        let argv = std::fs::read_to_string(dir.join("page.argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec![
                "-jpeg",
                "-r",
                "100",
                "-f",
                "1",
                "-l",
                "1",
                pdf.to_str().unwrap(),
                dir.join("page").to_str().unwrap(),
            ]
        );
    }

    #[test]
    fn the_renderer_sees_the_closures_environment_and_nothing_else() {
        // `env_clear` is the whole point of the gate: a tool installed on the CI box must never be
        // able to stand in for the closure's own copy, and neither may a variable of the CI box's
        // environment change how the closure's copy behaves.
        let leaked = std::env::vars()
            .find(|(k, v)| k != "PATH" && k != "HOME" && !v.is_empty())
            .expect("the test process has some environment to leak");
        let s = Scratch::new("render-env");
        let bin = s.bindir("bin");
        let dir = s.bindir("out");
        let pdf = s.join("f.pdf");
        std::fs::write(&pdf, fixture_pdf()).unwrap();
        s.stub(
            &bin,
            "pdftoppm",
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do out=\"$a\"; done\nprintf '%s\\n%s\\n%s\\n' \"$PATH\" \"$HOME\" \"${{{}-ABSENT}}\" > \"$out.env\"\nprintf '\\377\\330\\377' > \"$out-01.jpg\"\n",
                leaked.0
            ),
        );
        assert_eq!(render_from_closure(&closure_of(&bin), &pdf, &dir), Ok(1));
        let seen = std::fs::read_to_string(dir.join("page.env")).unwrap();
        let seen: Vec<&str> = seen.lines().collect();
        assert_eq!(
            seen[0],
            bin.to_str().unwrap(),
            "PATH is the closure's, only"
        );
        assert_eq!(seen[1], dir.to_str().unwrap(), "HOME is the scratch dir");
        assert_eq!(
            seen[2], "ABSENT",
            "{} reached the renderer — the gate is not isolated",
            leaked.0
        );
    }

    // ---- the capability surface, and the declared asymmetry ----

    #[test]
    fn a_capability_surface_counts_only_directories_nix_owns() {
        // A non-store entry on a runner's PATH is the ambient-acquisition bug a sibling gate
        // forbids; counting it here would let whatever a developer installed silence a real
        // asymmetry.
        let s = Scratch::new("surface");
        let store = s.bindir("store");
        let owned = store.join("abc-poppler/bin");
        std::fs::create_dir_all(&owned).unwrap();
        s.stub(&owned, "pdftoppm", "#!/bin/sh\n");
        let ambient = s.bindir("usr-local-bin");
        s.stub(&ambient, "pdftoppm-ambient", "#!/bin/sh\n");

        let path = std::ffi::OsString::from(format!(
            "{}:{}",
            owned.to_string_lossy(),
            ambient.to_string_lossy()
        ));
        let bins = capability_surface_under(&path, &store);
        assert!(bins.contains("pdftoppm"), "{bins:?}");
        assert!(!bins.contains("pdftoppm-ambient"), "{bins:?}");
    }

    #[test]
    fn matching_surfaces_have_no_faults() {
        let both = ["gh", "git", "pdftoppm", "jq"];
        let faults =
            surface_faults(&[surface("campaign-run", &both), surface("review-run", &both)]);
        // `jq` is declared campaign-only, so having it on BOTH sides makes that declaration stale.
        assert_eq!(
            faults,
            vec![ClosureFault::StaleAsymmetry {
                pkg: "campaign-run",
                bin: "jq"
            }]
        );
    }

    #[test]
    fn the_declared_asymmetry_is_the_only_difference_allowed() {
        assert_eq!(
            surface_faults(&[
                surface("campaign-run", &["gh", "git", "pdftoppm", "jq"]),
                surface("review-run", &["gh", "git", "pdftoppm"]),
            ]),
            vec![],
            "jq producer-only is exactly what DECLARED_ASYMMETRY says"
        );
    }

    #[test]
    fn an_undeclared_one_sided_binary_is_a_fault() {
        // #85's shape from the other side: a tool lands in one closure and not the other.
        let faults = surface_faults(&[
            surface("campaign-run", &["gh", "jq", "pdftoppm"]),
            surface("review-run", &["gh"]),
        ]);
        assert_eq!(
            faults,
            vec![ClosureFault::UndeclaredAsymmetry {
                pkg: "campaign-run".to_string(),
                bin: "pdftoppm".to_string()
            }]
        );
    }

    #[test]
    fn an_asymmetry_declared_for_one_runner_does_not_excuse_the_other() {
        // `jq` present ONLY to the vetter is not what the declaration says, and must not pass by
        // matching on the binary name alone.
        let faults = surface_faults(&[
            surface("campaign-run", &["gh"]),
            surface("review-run", &["gh", "jq"]),
        ]);
        assert!(
            faults.contains(&ClosureFault::UndeclaredAsymmetry {
                pkg: "review-run".to_string(),
                bin: "jq".to_string()
            }),
            "{faults:?}"
        );
        assert!(
            faults.contains(&ClosureFault::StaleAsymmetry {
                pkg: "campaign-run",
                bin: "jq"
            }),
            "{faults:?}"
        );
    }

    #[test]
    fn a_declaration_naming_no_compared_runner_is_stale() {
        // The producer package gets renamed and the declaration is not updated: the surfaces
        // themselves are clean, and the entry now exempts nothing while looking like it exempts
        // something.
        assert_eq!(
            surface_faults(&[
                surface("producer-run", &["gh", "jq"]),
                surface("review-run", &["gh", "jq"]),
            ]),
            vec![ClosureFault::StaleAsymmetry {
                pkg: "campaign-run",
                bin: "jq"
            }]
        );
    }

    #[test]
    fn an_empty_surface_is_never_symmetric() {
        // Two empty sets compare equal, so without this the gate would pass loudest exactly when
        // a closure was most broken.
        let faults = surface_faults(&[surface("campaign-run", &[]), surface("review-run", &[])]);
        assert!(
            faults.contains(&ClosureFault::EmptySurface {
                pkg: "campaign-run".to_string()
            }),
            "{faults:?}"
        );
        assert!(
            faults.contains(&ClosureFault::EmptySurface {
                pkg: "review-run".to_string()
            }),
            "{faults:?}"
        );
        assert_eq!(closure_exit_code(&faults), CLOSURE_UNSATISFIED);
    }

    #[test]
    fn nothing_to_compare_produces_no_asymmetry_faults() {
        // Every closure failed to build. The caller already holds that fault, and "every
        // declaration is stale" on top of it would name the wrong file.
        assert_eq!(surface_faults(&[]), vec![]);
    }

    // ---- every fault says what to do about it ----

    #[test]
    fn every_fault_names_its_package_and_what_to_change() {
        let faults = [
            ClosureFault::Unbuildable {
                pkg: "review-run".to_string(),
                detail: "no such output".to_string(),
            },
            ClosureFault::NoBakedPath {
                pkg: "review-run".to_string(),
                script: "/nix/store/x/bin/review-run".to_string(),
            },
            ClosureFault::ToolMissing {
                pkg: "review-run".to_string(),
                bin: "pdftoppm",
                why: "Read renders PDF pages",
            },
            ClosureFault::RenderFailed {
                pkg: "review-run".to_string(),
                how: RenderFault::NoPages,
            },
            ClosureFault::UndeclaredAsymmetry {
                pkg: "review-run".to_string(),
                bin: "jq".to_string(),
            },
            ClosureFault::EmptySurface {
                pkg: "review-run".to_string(),
            },
        ];
        for f in &faults {
            let m = f.describe();
            assert!(m.contains("review-run"), "{m}");
            assert!(m.len() > 40, "a fault must be actionable, not a word: {m}");
        }
    }

    #[test]
    fn a_render_fault_says_which_of_the_five_ways_it_failed() {
        // The diagnosis is derived from the variant, never from re-reading a rendered string.
        for (f, expect) in [
            (RenderFault::NotInClosure, "not on the runner's baked PATH"),
            (
                RenderFault::Unspawnable(std::io::ErrorKind::PermissionDenied),
                "would not start",
            ),
            (
                RenderFault::Exited {
                    code: Some(127),
                    stderr: "boom".to_string(),
                },
                "exited 127",
            ),
            (RenderFault::NoPages, "wrote no pages"),
            (
                RenderFault::NotJpeg {
                    page: "/t/page-01.jpg".to_string(),
                    magic: vec![0x25, 0x50, 0x44],
                },
                "not a JPEG",
            ),
        ] {
            assert!(f.describe().contains(expect), "{}", f.describe());
        }
        assert_eq!(
            RenderFault::Exited {
                code: None,
                stderr: String::new()
            }
            .describe(),
            "`pdftoppm` exited on a signal: (no stderr)",
            "a signal death is not exit 0"
        );
    }
}
