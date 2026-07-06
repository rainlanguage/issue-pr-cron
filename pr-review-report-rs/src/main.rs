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
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Ready,
    Relink,
    Reject,
    Close,
    Unknown,
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Human,
    AiCampaign,
    Other,
}

#[derive(Clone, Copy, PartialEq)]
enum Bucket {
    Approved,
    AiVet,
    StaleVet,
    ProducerFix,
    Relink,
    Reject,
    Close,
    Unreviewed,
    Conflicting,
    Pending,
    Draft,
    FetchError,
    UnknownVerdict,
}
impl Bucket {
    fn key(self) -> &'static str {
        match self {
            Bucket::Approved => "APPROVED",
            Bucket::AiVet => "AIVET",
            Bucket::StaleVet => "STALE_VET",
            Bucket::ProducerFix => "PRODUCER_FIX",
            Bucket::Relink => "RELINK",
            Bucket::Reject => "REJECT",
            Bucket::Close => "CLOSE",
            Bucket::Unreviewed => "UNREVIEWED",
            Bucket::Conflicting => "CONFLICTING",
            Bucket::Pending => "PENDING",
            Bucket::Draft => "DRAFT",
            Bucket::FetchError => "FETCH_ERROR",
            Bucket::UnknownVerdict => "UNKNOWN_VERDICT",
        }
    }
}

/// Replace tab/newline/cr with a space and truncate to `n` Unicode codepoints (matches jq `.[0:n]`).
fn sanitize(s: &str, n: usize) -> String {
    s.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .take(n)
        .collect()
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

/// Append one line (+newline) to a file, creating it if absent — the append-only cost sidecar write.
fn append_line(path: &str, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

struct VEntry {
    verdict: Option<Verdict>,
    source: Source,
    sha: String,
    note: String,
    cost: Option<i64>,
    cost_basis: String,
}

/// FIX(bug 1,6,7): parse the ledger line-by-line so one malformed line (e.g. `hello`) can't drop
/// every verdict after it; normalize (trim+lowercase) the verdict; last matching line wins per key.
fn load_verdicts(path: &str) -> HashMap<String, VEntry> {
    parse_verdicts(&std::fs::read_to_string(path).unwrap_or_default())
}

fn parse_verdicts(content: &str) -> HashMap<String, VEntry> {
    let mut m = HashMap::new();
    for line in content.lines() {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !v.is_object() {
            continue;
        }
        let repo = match v.get("repo").and_then(|x| x.as_str()) {
            Some(r) => r,
            None => continue,
        };
        let pr = match v.get("pr") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let key = format!("{repo}/{pr}");
        let raw = v
            .get("verdict")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let verdict = match raw.as_str() {
            "" => None,
            "ready" => Some(Verdict::Ready),
            "relink" => Some(Verdict::Relink),
            "reject" => Some(Verdict::Reject),
            "close" => Some(Verdict::Close),
            _ => Some(Verdict::Unknown),
        };
        let source = match v.get("source").and_then(|x| x.as_str()) {
            Some("human") => Source::Human,
            Some("ai-campaign") | None => Source::AiCampaign,
            Some(_) => Source::Other,
        };
        let sha = v
            .get("sha")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let note = sanitize(v.get("note").and_then(|x| x.as_str()).unwrap_or(""), 100);
        // Cost is stamped by the vetter as a number; tolerate a numeric string too (matches the
        // python queue sort, which accepted either via int()).
        let cost = match v.get("cost") {
            Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        let cost_basis = sanitize(
            v.get("cost_basis").and_then(|x| x.as_str()).unwrap_or(""),
            120,
        );
        m.insert(
            key,
            VEntry {
                verdict,
                source,
                sha,
                note,
                cost,
                cost_basis,
            },
        );
    }
    m
}

struct PrRow {
    repo: String,
    num: u64,
    merge: Merge,
    ci: Ci,
    draft: bool,
    rev: Option<String>,
    url: String,
    headoid: String,
    title: String,
    fetch_error: bool,
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

/// FIX(bug 3,4,5): on gh failure, flag fetch_error (→ a visible FETCH_ERROR bucket) instead of
/// emitting a `?` row that silently becomes PENDING. URL falls back to the constructed PR url.
fn classify_one(org: &str, repo: &str, num: u64) -> PrRow {
    let target = format!("{org}/{repo}");
    let url_fb = format!("https://github.com/{org}/{repo}/pull/{num}");
    let j = gh_json(&[
        "pr",
        "view",
        &num.to_string(),
        "-R",
        &target,
        "--json",
        "url,mergeable,isDraft,reviewDecision,statusCheckRollup,headRefOid,title",
    ]);
    match j {
        None => PrRow {
            repo: repo.to_string(),
            num,
            merge: Merge::Unknown,
            ci: Ci::NoChecks,
            draft: false,
            rev: None,
            url: url_fb,
            headoid: "-".to_string(),
            title: String::new(),
            fetch_error: true,
        },
        Some(j) => {
            let url = j
                .get("url")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&url_fb)
                .to_string();
            let merge = match j.get("mergeable").and_then(|x| x.as_str()) {
                Some("MERGEABLE") => Merge::Mergeable,
                Some("CONFLICTING") => Merge::Conflicting,
                _ => Merge::Unknown,
            };
            let draft = j.get("isDraft").and_then(|x| x.as_bool()).unwrap_or(false);
            let rev = j
                .get("reviewDecision")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let ci = classify_ci(j.get("statusCheckRollup").unwrap_or(&Value::Null));
            let headoid = j
                .get("headRefOid")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("-")
                .to_string();
            let title = sanitize(j.get("title").and_then(|x| x.as_str()).unwrap_or(""), 100);
            PrRow {
                repo: repo.to_string(),
                num,
                merge,
                ci,
                draft,
                rev,
                url,
                headoid,
                title,
                fetch_error: false,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn bucket_of(
    e: Option<&VEntry>,
    rev: Option<&str>,
    merge: Merge,
    ci: Ci,
    draft: bool,
    headoid: &str,
    fetch_error: bool,
) -> Bucket {
    let verdict = e.and_then(|x| x.verdict);
    // An AI/human-flagged disposition IS the action, regardless of CI or fetch state.
    match verdict {
        Some(Verdict::Close) => return Bucket::Close,
        Some(Verdict::Reject) => return Bucket::Reject,
        Some(Verdict::Relink) => return Bucket::Relink,
        Some(Verdict::Unknown) => return Bucket::UnknownVerdict, // FIX(bug 6,7): surface, don't silently UNREVIEWED
        _ => {}
    }
    // FIX(rs-bug 1): a transient fetch failure masks only state-dependent buckets, AFTER the
    // state-independent verdict disposition above (matches the original bash precedence).
    if fetch_error {
        return Bucket::FetchError;
    }
    if rev == Some("CHANGES_REQUESTED") {
        return Bucket::Reject;
    }
    if draft {
        return Bucket::Draft;
    }
    if ci == Ci::Red {
        return Bucket::ProducerFix;
    }
    if merge == Merge::Conflicting {
        return Bucket::Conflicting;
    }
    if ci == Ci::Pending {
        return Bucket::Pending;
    }
    let is_ready = verdict == Some(Verdict::Ready);
    let src = e.map(|x| x.source);
    let approved = rev == Some("APPROVED") || (is_ready && src == Some(Source::Human));
    let aivet = !approved && is_ready && src == Some(Source::AiCampaign);
    if merge == Merge::Mergeable && (ci == Ci::Green || ci == Ci::NoChecks) {
        let vsha = e.map(|x| x.sha.as_str()).unwrap_or("");
        // FIX(rs-bug 2): a recorded verdict with a real sha is STALE whenever the live head can't be
        // confirmed equal — including a missing/unknown head ("-") — not only on an explicit mismatch.
        if (approved || aivet)
            && !vsha.is_empty()
            && vsha != "-"
            && (headoid.is_empty() || headoid == "-" || vsha != headoid)
        {
            return Bucket::StaleVet;
        }
        if approved {
            return Bucket::Approved;
        }
        if aivet {
            return Bucket::AiVet;
        }
        return Bucket::Unreviewed;
    }
    Bucket::Pending // mergeability still resolving (mergeable=UNKNOWN)
}

struct OutRow {
    bucket: Bucket,
    url: String,
    oneliner: String,
}

fn cfg(cron: &HashMap<String, String>, env_key: &str, def: &str) -> String {
    // FIX(rs-bug 5): a set-but-EMPTY env OR cron.env value falls back to the default (bash ${VAR:-def}).
    std::env::var(env_key)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cron.get(env_key).cloned().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| def.to_string())
}

/// FIX(rs-bug 6): resolve the base dir (where the ledgers live) like bash's $DIR, so the binary finds
/// them regardless of CWD: RAINIX_BATCH_DIR env, else walk up from the exe to the first dir holding a
/// ledger / cron.env, else ".".
fn base_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("RAINIX_BATCH_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..6 {
            let Some(d) = dir else { break };
            if d.join("review-verdicts.jsonl").exists()
                || d.join("cron.env").exists()
                || d.join("close-candidates.jsonl").exists()
            {
                return d;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    std::path::PathBuf::from(".")
}

/// `--queue [N]`: print the human review queue sorted by verification cost (cheapest first).
/// Queue = every OPEN, non-draft PR in the covered orgs whose effective (last-wins-by-position)
/// verdict is ready/ai-campaign. Cost from the verdict line's own `cost`, else the
/// review-costs.jsonl sidecar (sha-matching preferred, mismatched sha flagged), else unscored
/// (sorts last at 1001). N defaults to 20; 0 = all.
/// Sidecar backfill costs: key -> (cost, basis, sha). Same line-by-line tolerance as the ledger.
fn parse_sidecar(content: &str) -> HashMap<String, (i64, String, String)> {
    let mut sidecar = HashMap::new();
    for line in content.lines() {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(repo) = v.get("repo").and_then(|x| x.as_str()) else {
            continue;
        };
        let pr = match v.get("pr") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let Some(cost) = v.get("cost").and_then(|x| x.as_i64()) else {
            continue;
        };
        let basis = sanitize(v.get("basis").and_then(|x| x.as_str()).unwrap_or(""), 120);
        let sha = v
            .get("sha")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        sidecar.insert(format!("{repo}/{pr}"), (cost, basis, sha));
    }
    sidecar
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

/// owner/repo slug + number → verdict-ledger / cost-sidecar key: bare repo name for rainlanguage,
/// org-qualified "owner/repo" for every other org (e.g. cyclofinance/cyclo.site) — the convention
/// those files use.
fn ledger_key(slug: &str, num: u64) -> String {
    let repo = slug.strip_prefix("rainlanguage/").unwrap_or(slug);
    format!("{repo}/{num}")
}

/// Verification cost + basis for ordering the queue cheapest-first: the verdict line's own cost
/// wins, else the sidecar's (sha mismatch flagged), else unscored (1001, sorts last). A label-only
/// candidate with no ledger entry falls straight through to the sidecar, then unscored.
fn cost_for(
    key: &str,
    verds: &HashMap<String, VEntry>,
    sidecar: &HashMap<String, (i64, String, String)>,
) -> (i64, String) {
    if let Some(v) = verds.get(key) {
        if let Some(c) = v.cost {
            return (c, v.cost_basis.clone());
        }
        if let Some((c, b, sha)) = sidecar.get(key) {
            let stale = if !sha.is_empty() && !v.sha.is_empty() && sha != &v.sha {
                " [cost from older head]"
            } else {
                ""
            };
            return (*c, format!("{b}{stale}"));
        }
        return (1001, String::new());
    }
    if let Some((c, b, _)) = sidecar.get(key) {
        return (*c, b.clone());
    }
    (1001, String::new())
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
        "review queue: {} ai:ready -> {} presentable, {} conflicting, {} red, {} pending, {} unknown-merge, {} approved{}{} (cheapest first){}\n",
        c.raw, rows.len(), c.conflict, c.red, c.pending, c.merge_unknown, c.approved, err, excl, trunc
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

fn queue_mode(review_verdicts: &str, costs_path: &str, top: usize) {
    let verds = load_verdicts(review_verdicts);
    let sidecar = parse_sidecar(&std::fs::read_to_string(costs_path).unwrap_or_default());

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
            "mergeable,statusCheckRollup,reviewDecision",
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
                let (cost, basis) = cost_for(&ledger_key(slug, *num), &verds, &sidecar);
                let repo_disp = slug.rsplit('/').next().unwrap_or(slug).to_string();
                rows.push((cost, repo_disp, *num, url.clone(), basis));
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

/// Parse cron.env content (KEY=VALUE lines): skips blanks/comments, strips `export `, honours
/// double/single quotes, strips a trailing unquoted ` #...` comment, expands leading $HOME/~.
fn parse_cron_env(content: &str) -> HashMap<String, String> {
    let mut cron: HashMap<String, String> = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v0)) = line.split_once('=') {
            let v0 = v0.trim_start();
            // FIX(rs-bug 7): honour quotes; strip a trailing unquoted ` #...` comment; expand $HOME/~.
            let val = if let Some(rest) = v0.strip_prefix('"') {
                rest.split_once('"')
                    .map(|(inner, _)| inner.to_string())
                    .unwrap_or_else(|| rest.to_string())
            } else if let Some(rest) = v0.strip_prefix('\'') {
                rest.split_once('\'')
                    .map(|(inner, _)| inner.to_string())
                    .unwrap_or_else(|| rest.to_string())
            } else {
                let cut = v0.find(" #").or_else(|| v0.find("\t#")).unwrap_or(v0.len());
                v0[..cut].trim().to_string()
            };
            let val = if let Some(rest) = val.strip_prefix("$HOME") {
                format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
            } else if let Some(rest) = val.strip_prefix("~/") {
                format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
            } else {
                val
            };
            cron.insert(k.trim().to_string(), val);
        }
    }
    cron
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

/// The SHA-bound vetter comment: `🤖 ai:vetter` marker line, then `Reviewed <sha>: <verdict>`
/// (plus ` — <note>` when a note is given).
fn verdict_comment(sha: &str, verdict: &str, note: &str) -> String {
    let tail = if note.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", note.trim())
    };
    format!("🤖 ai:vetter\nReviewed {sha}: {verdict}{tail}")
}

/// Body of the PR's most-recent comment starting with the `🤖 ai:vetter` marker (chronological
/// `comments` array, last match wins), or None.
fn last_vetter_comment(pr: &Value) -> Option<String> {
    pr.get("comments")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("body").and_then(|b| b.as_str()))
        .rfind(|b| b.starts_with("🤖 ai:vetter"))
        .map(String::from)
}

/// Skip a new vetter comment iff the last one already recorded the SAME verdict at the SAME head sha
/// (no-op re-review). A moved head or a changed verdict does NOT skip.
fn should_skip_comment(last_vetter_body: Option<&str>, sha: &str, verdict: &str) -> bool {
    match last_vetter_body {
        Some(b) => b.contains(&format!("Reviewed {sha}: {verdict}")),
        None => false,
    }
}

/// One cost-sidecar line (review-costs.jsonl) in the shape `cost_for`/`parse_sidecar` reads: the
/// ledger-key repo (bare for rainlanguage, org-qualified otherwise), pr, cost, basis, reviewed sha.
/// This keeps the queue's cheapest-first ordering working once the vetter writes labels not a ledger.
fn cost_sidecar_line(ledger_repo: &str, pr: &str, cost: i64, basis: &str, sha: &str) -> String {
    serde_json::json!({"repo": ledger_repo, "pr": pr, "cost": cost, "basis": basis, "sha": sha})
        .to_string()
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
    // Sacred: never override a human verdict. This is the guard whose ABSENCE a mutation must fail.
    if has_human_override(pr_json) {
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
    costs_path: &str,
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
        "headRefOid,labels,comments",
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
    let comment = verdict_comment(&sha, verdict, note);

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
                Some(c) => format!("{c} ({basis}) -> {costs_path}"),
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
    if !skip && !gh_run(&["pr", "comment", pr, "-R", slug, "--body", &comment]) {
        eprintln!("error: recorded {target} on {slug}#{pr} but FAILED to post the verdict comment");
        return 1;
    }
    // Cost feeds the queue's cheapest-first ordering (cost_for reads this sidecar).
    if let Some(c) = cost {
        let ledger_repo = slug.strip_prefix("rainlanguage/").unwrap_or(slug);
        let line = cost_sidecar_line(ledger_repo, pr, c, basis, &sha);
        if let Err(e) = append_line(costs_path, &line) {
            eprintln!(
                "warning: recorded {target} on {slug}#{pr} but failed to write cost to {costs_path}: {e}"
            );
        }
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

fn main() {
    let base = base_dir();
    // Parse cron.env (KEY=VALUE) from the base dir if present; env vars override it; then defaults.
    let cron = parse_cron_env(&std::fs::read_to_string(base.join("cron.env")).unwrap_or_default());
    let org = cfg(&cron, "ORG", "rainlanguage");
    // FIX(rs-bug 5): PR_ASSIGNEE via cfg() so a set-but-empty value also falls back to the default.
    let author = cfg(&cron, "PR_ASSIGNEE", "thedavidmeister");
    // FIX(rs-bug 6): default the ledgers to the base dir (found regardless of CWD).
    let cc_def = base
        .join("close-candidates.jsonl")
        .to_string_lossy()
        .into_owned();
    let rv_def = base
        .join("review-verdicts.jsonl")
        .to_string_lossy()
        .into_owned();
    let close_candidates = cfg(&cron, "CLOSE_CANDIDATES", &cc_def);
    let review_verdicts = cfg(&cron, "REVIEW_VERDICTS", &rv_def);
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--queue") {
        let top = args
            .get(2)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);
        let costs_def = base
            .join("review-costs.jsonl")
            .to_string_lossy()
            .into_owned();
        let costs_path = cfg(&cron, "REVIEW_COSTS", &costs_def);
        queue_mode(&review_verdicts, &costs_path, top);
        return;
    }
    if args.get(1).map(String::as_str) == Some("--commit-closes") {
        let (Some(slug), Some(pr)) = (args.get(2), args.get(3)) else {
            eprintln!("usage: pr-review-report --commit-closes <owner/repo> <pr>");
            std::process::exit(2);
        };
        std::process::exit(commit_closes_mode(slug, pr));
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
        let costs_def = base
            .join("review-costs.jsonl")
            .to_string_lossy()
            .into_owned();
        let costs_path = cfg(&cron, "REVIEW_COSTS", &costs_def);
        std::process::exit(record_verdict_mode(
            slug,
            pr,
            verdict,
            &note,
            cost,
            &basis,
            &costs_path,
            dry_run,
        ));
    }
    let only_ready = args.get(1).map(String::as_str) == Some("--ready");

    let now = Command::new("date")
        .args(["-u", "+%FT%TZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("PR review report — {}, author {} — {}", org, author, now);
    println!("pipeline:  🟦 unreviewed  →  🤖 AI-vetted  →  ✅ you approve  →  merge");
    println!("(AI verdicts: review-verdicts.jsonl · your sign-off: a GitHub approval, or a verdict with source=human)");
    println!("================================================================");

    // FIX(bug 4): a failed `gh search prs` aborts loudly — never a falsely-empty all-clear.
    let search = Command::new("gh")
        .args([
            "search",
            "prs",
            "--owner",
            &org,
            "--author",
            &author,
            "--state",
            "open",
            "--limit",
            "300",
            "--json",
            "repository,number",
        ])
        .output();
    let out = match search {
        Ok(o) if o.status.success() => o,
        _ => {
            eprintln!("error: `gh search prs` failed (transient API error / auth?) — aborting rather than print a falsely-empty report");
            std::process::exit(1);
        }
    };
    let val: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: could not parse `gh search prs` output: {e}");
            std::process::exit(1);
        }
    };
    // FIX(rs-bug 4): a success response that parses but isn't an array is a malformed fetch — abort
    // loudly rather than fold it into an empty Vec (a falsely-empty all-clear).
    let arr = match val.as_array() {
        Some(a) => a,
        None => {
            eprintln!("error: `gh search prs` returned non-array JSON — aborting rather than print a falsely-empty report");
            std::process::exit(1);
        }
    };
    let prs: Vec<(String, u64)> = arr
        .iter()
        .filter_map(|x| {
            let repo = x
                .get("repository")
                .and_then(|r| r.get("name"))
                .and_then(|n| n.as_str())?
                .to_string();
            let num = x.get("number").and_then(|n| n.as_u64())?;
            Some((repo, num))
        })
        .collect();

    let verds = load_verdicts(&review_verdicts);

    // Bounded-concurrency per-PR fan-out (~12 workers, std only) via scoped threads + an atomic cursor.
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<PrRow>> = Mutex::new(Vec::with_capacity(prs.len()));
    let nworkers = prs.len().clamp(1, 12);
    std::thread::scope(|s| {
        for _ in 0..nworkers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= prs.len() {
                    break;
                }
                let (repo, num) = &prs[i];
                let row = classify_one(&org, repo, *num);
                results.lock().unwrap().push(row);
            });
        }
    });
    let rows = results.into_inner().unwrap();

    let mut outs: Vec<OutRow> = Vec::with_capacity(rows.len());
    for r in &rows {
        let key = format!("{}/{}", r.repo, r.num);
        let e = verds.get(&key);
        let b = bucket_of(
            e,
            r.rev.as_deref(),
            r.merge,
            r.ci,
            r.draft,
            &r.headoid,
            r.fetch_error,
        );
        let note = e.map(|x| x.note.clone()).unwrap_or_default();
        let oneliner = if note.is_empty() || note == "approved by user" {
            if r.title.is_empty() {
                if r.fetch_error {
                    "(gh pr view failed — state unknown)".to_string()
                } else {
                    "?".to_string()
                }
            } else {
                r.title.clone()
            }
        } else {
            note
        };
        outs.push(OutRow {
            bucket: b,
            url: r.url.clone(),
            oneliner,
        });
    }

    let emit = |bucket: Bucket, header: &str| {
        let mut lines: Vec<String> = outs
            .iter()
            .filter(|o| o.bucket == bucket)
            .map(|o| format!("  {}  —  {}", o.url, o.oneliner))
            .collect();
        if lines.is_empty() {
            return;
        }
        lines.sort();
        println!();
        println!("{}  ({})", header, lines.len());
        for l in &lines {
            println!("{}", l);
        }
    };

    emit(
        Bucket::Approved,
        "✅ APPROVED BY YOU — ready to merge (GitHub approval / verdict you set)",
    );
    if only_ready {
        return;
    }
    emit(Bucket::AiVet, "🤖 AI-VETTED — awaiting YOUR approval (passed the automated review; NOT human-reviewed yet)");
    emit(Bucket::StaleVet, "🔄 RE-VET PENDING — head moved since the recorded verdict (e.g. a producer step-3b fix); the vetter re-reviews the new commit before this can merge");
    emit(Bucket::ProducerFix, "🔴 RED — NEEDS A PRODUCER FIX (CI failing; the producer cron diagnoses it and pushes a fix to drive it green — producer work, NOT 'blocked', NOT your action)");
    emit(
        Bucket::Relink,
        "🔧 AI-flagged — relink Closes→Refs before merge (else it auto-closes a live issue)",
    );
    emit(
        Bucket::Reject,
        "❌ AI-flagged / you requested changes — rework or close",
    );
    emit(
        Bucket::Close,
        "🗑️  AI-flagged — close (duplicate / superseded)",
    );
    emit(
        Bucket::Unreviewed,
        "🟦 NOT YET REVIEWED — green + mergeable, awaiting AI review + your approval",
    );
    emit(
        Bucket::Conflicting,
        "⚠️  CONFLICTING — needs a rebase onto current main (producer work)",
    );
    emit(
        Bucket::Pending,
        "🟡 PENDING — CI / mergeability still resolving (no action; just wait)",
    );
    emit(Bucket::Draft, "📝 DRAFTS — intentionally not ready");
    // FIX(bug 3,5): degraded/partial data is visible, not masked as a settled PENDING.
    emit(Bucket::FetchError, "⚠️  COULD NOT FETCH — gh pr view failed; state UNKNOWN (re-run; NOT a settled 'just wait')");
    emit(Bucket::UnknownVerdict, "❓ UNKNOWN VERDICT — ledger has a non-canonical verdict (expected ready|relink|reject|close); fix the ledger line");

    close_candidates_section(&org, &close_candidates);

    println!();
    println!("----------------------------------------------------------------");
    println!("totals: {} open PRs by {}  ·  buckets:", rows.len(), author);
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for o in &outs {
        *counts.entry(o.bucket.key()).or_insert(0) += 1;
    }
    let mut hv: Vec<(&str, usize)> = counts.into_iter().collect();
    hv.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (k, c) in hv {
        println!("   {} {}", c, k);
    }
}

/// FIX(bug 1,8,9,10,11 + rs-bug 8): robust line-by-line parse; require a string repo; dedup on the
/// (repo,issue) identity (not the url, so an explicit url vs the constructed fallback for the same
/// issue can't double-count); classify each reason as NOT-LANDED (covered/made-moot by an OPEN pr →
/// exclude + counted separately), LANDED (show), or UNRECOGNIZED (show, tagged — never silently
/// dropped); FAIL-OPEN on the live state check (gh error → state UNKNOWN, still SHOWN).
fn close_candidates_section(org: &str, path: &str) {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    if content.trim().is_empty() {
        return;
    }
    let mut latest: HashMap<String, (String, String, String, String)> = HashMap::new(); // id -> (full, issue, url, reason)
    for line in content.lines() {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !v.is_object() {
            continue;
        }
        if v.get("issue").map(|x| x.is_null()).unwrap_or(true) {
            continue;
        }
        let issue = match v.get("issue") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let repo = match v.get("repo").and_then(|x| x.as_str()) {
            Some(r) => r.to_string(),
            None => continue,
        };
        let full = if repo.contains('/') {
            repo
        } else {
            format!("{org}/{repo}")
        };
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://github.com/{full}/issues/{issue}"));
        let reason = sanitize(
            v.get("reason")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("note").and_then(|x| x.as_str()))
                .unwrap_or(""),
            80,
        );
        let id = format!("{full}#{issue}");
        latest.insert(id, (full, issue, url, reason));
    }
    let mut items: Vec<(String, String, String, String)> = latest.into_values().collect();
    items.sort_by(|a, b| a.2.cmp(&b.2)); // by url, for display

    let mut open_c = 0usize;
    let mut closed_c = 0usize;
    let mut unknown_c = 0usize;
    let mut not_landed_c = 0usize;
    let mut rows: Vec<String> = Vec::new();
    for (full, issue, url, reason) in items {
        let rl = reason.trim().to_lowercase();
        // NOT LANDED: covered/made-moot by an OPEN pr/dependency — the fix hasn't merged, so this is
        // not a manual close-candidate (it self-closes on merge via `Closes #N`, or stays open). Exclude.
        let not_landed = rl.contains("open-pr")
            || rl.contains("open pr")
            || rl.contains("covered-by-open")
            || rl.contains("made-moot-by")
            || rl.contains("opened to")
            || rl.contains("pr opened")
            || rl.contains("opened a pr");
        if not_landed {
            not_landed_c += 1;
            continue;
        }
        // LANDED: the leading token is a canonical landed reason, OR a won't-fix token appears anywhere.
        let lead = rl.split([' ', ':']).next().unwrap_or("");
        let landed = matches!(
            lead,
            "already-fixed-on-main"
                | "already-addressed-by-ci"
                | "invalid"
                | "duplicate"
                | "wont-fix"
                | "won't-fix"
                | "wontfix"
        ) || rl.contains("wontfix")
            || rl.contains("won't-fix")
            || rl.contains("wont-fix")
            || rl.contains("won't fix")
            || rl.contains("wont fix");
        // UNRECOGNIZED (neither not-landed nor landed) is still SHOWN, tagged — never silently dropped.
        let tag = if landed {
            ""
        } else {
            "  [reason unrecognized — verify]"
        };
        let st =
            gh_json(&["issue", "view", &issue, "-R", &full, "--json", "state"]).and_then(|j| {
                j.get("state")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            });
        match st.as_deref() {
            Some("OPEN") => {
                open_c += 1;
                rows.push(format!("  {}  — {}{}", url, reason, tag));
            }
            Some("CLOSED") => {
                closed_c += 1;
            }
            _ => {
                unknown_c += 1;
                rows.push(format!(
                    "  {}  — {} (state UNKNOWN — gh error){}",
                    url, reason, tag
                ));
            }
        }
    }
    if open_c + unknown_c > 0 {
        rows.sort();
        rows.dedup();
        println!();
        println!(
            "🗑️  ISSUE CLOSE-CANDIDATES — cron-logged landed-fix/invalid issues still OPEN ({} open, {} unverified shown; {} already closed, hidden; {} not-landed/open-pr-covered, excluded)",
            open_c, unknown_c, closed_c, not_landed_c
        );
        for r in &rows {
            println!("{}", r);
        }
    }
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

    // --- ledger_key: bare for rainlanguage, org-qualified otherwise -----------------------------

    #[test]
    fn ledger_key_bare_for_rainlanguage_qualified_for_others() {
        assert_eq!(ledger_key("rainlanguage/rainix", 5), "rainix/5");
        assert_eq!(
            ledger_key("cyclofinance/cyclo.site", 400),
            "cyclofinance/cyclo.site/400"
        );
    }

    // --- cost_for: cheapest-first ordering signal ----------------------------------------------

    // The verdict line's own cost + basis wins over the sidecar.
    #[test]
    fn verdict_cost_beats_sidecar() {
        let v = parse_verdicts(
            r#"{"repo":"r","pr":1,"verdict":"ready","source":"ai-campaign","sha":"aaa","cost":60,"cost_basis":"basis-1"}"#,
        );
        let side = parse_sidecar(r#"{"repo":"r","pr":1,"cost":999,"basis":"side","sha":"aaa"}"#);
        assert_eq!(cost_for("r/1", &v, &side), (60, "basis-1".to_string()));
    }

    // No cost on the verdict falls to the sidecar; the stale flag appears ONLY when both shas are
    // non-empty and differ.
    #[test]
    fn sidecar_cost_and_stale_flag() {
        let v = parse_verdicts(
            r#"{"repo":"r","pr":1,"verdict":"ready","source":"ai-campaign","sha":"vvv"}"#,
        );
        let diff = parse_sidecar(r#"{"repo":"r","pr":1,"cost":5,"basis":"b","sha":"other"}"#);
        assert_eq!(
            cost_for("r/1", &v, &diff),
            (5, "b [cost from older head]".to_string())
        );
        let same = parse_sidecar(r#"{"repo":"r","pr":1,"cost":5,"basis":"b","sha":"vvv"}"#);
        assert_eq!(cost_for("r/1", &v, &same), (5, "b".to_string()));
    }

    // A label-only candidate with NO ledger verdict still costs from the sidecar (new path).
    #[test]
    fn label_only_candidate_costs_from_sidecar() {
        let side = parse_sidecar(r#"{"repo":"r","pr":1,"cost":7,"basis":"s","sha":"x"}"#);
        assert_eq!(
            cost_for("r/1", &HashMap::new(), &side),
            (7, "s".to_string())
        );
    }

    // Nothing anywhere => unscored 1001 (sorts last).
    #[test]
    fn unscored_when_no_cost_anywhere() {
        assert_eq!(
            cost_for("r/1", &HashMap::new(), &HashMap::new()),
            (1001, String::new())
        );
    }

    // --- parse_verdicts: still used by the queue's cost lookup + the full report ----------------

    // Last matching line wins by position (append-only ledger).
    #[test]
    fn parse_verdicts_last_line_wins() {
        let led = format!(
            "{}\n{}",
            r#"{"repo":"r","pr":1,"verdict":"ready","source":"ai-campaign"}"#,
            r#"{"repo":"r","pr":1,"verdict":"reject","source":"ai-campaign"}"#
        );
        assert!(parse_verdicts(&led)["r/1"].verdict == Some(Verdict::Reject));
    }

    // A `pr` written as a JSON string keys identically to the numeric form.
    #[test]
    fn parse_verdicts_pr_as_string_keys_same() {
        let v = parse_verdicts(r#"{"repo":"r","pr":"7","verdict":"ready","source":"ai-campaign"}"#);
        assert!(v.contains_key("r/7"));
    }

    // One malformed line must not drop the lines after it.
    #[test]
    fn parse_verdicts_malformed_line_skipped() {
        let led = format!(
            "not json\n{}",
            r#"{"repo":"r","pr":1,"verdict":"ready","source":"ai-campaign"}"#
        );
        assert!(parse_verdicts(&led)["r/1"].verdict == Some(Verdict::Ready));
    }

    // Verdict matching is trimmed + case-insensitive.
    #[test]
    fn parse_verdicts_verdict_normalized() {
        let v =
            parse_verdicts(r#"{"repo":"r","pr":1,"verdict":"  Ready ","source":"ai-campaign"}"#);
        assert!(v["r/1"].verdict == Some(Verdict::Ready));
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
                "review queue: 5 ai:ready -> 1 presentable, 2 conflicting, 1 red, 0 pending, 0 unknown-merge, 1 approved (cheapest first)\n"
            ),
            "header:\n{out}"
        );
        assert!(out
            .contains("\n    60  r#1  basis-1\n        https://github.com/rainlanguage/r/pull/1"));
    }

    // Unscored rows render "unscored"; excluded + fetch-error surface in the header.
    #[test]
    fn render_unscored_and_notes() {
        let rows: Vec<QueueRow> = vec![(1001, "r".to_string(), 2, "u".to_string(), String::new())];
        let mut c = qc(3, 0, 0, 0, 0);
        c.excluded = 1;
        c.fetch_error = 1;
        c.merge_unknown = 2;
        let out = render_queue(&rows, &c, 0);
        assert!(out.contains("  unscored  r#2  "), "unscored:\n{out}");
        assert!(out.contains("1 fetch-error"));
        assert!(out.contains("1 excluded (draft/human-override)"));
        assert!(
            out.contains("2 unknown-merge"),
            "unknown-merge count:\n{out}"
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

    fn ventry(verdict: Option<Verdict>, source: Source, sha: &str) -> VEntry {
        VEntry {
            verdict,
            source,
            sha: sha.to_string(),
            note: String::new(),
            cost: None,
            cost_basis: String::new(),
        }
    }

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

    // K1: a recorded disposition (close/reject/relink/unknown) IS the bucket, regardless of
    // fetch errors, draft state, or CI — state-independent precedence.
    #[test]
    fn bucket_disposition_beats_everything() {
        let e = ventry(Some(Verdict::Close), Source::AiCampaign, "");
        assert!(
            bucket_of(Some(&e), None, Merge::Conflicting, Ci::Red, true, "-", true)
                == Bucket::Close
        );
        let e = ventry(Some(Verdict::Reject), Source::Human, "");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Unknown,
                Ci::Pending,
                false,
                "-",
                true
            ) == Bucket::Reject
        );
        let e = ventry(Some(Verdict::Relink), Source::AiCampaign, "");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "x",
                false
            ) == Bucket::Relink
        );
        let e = ventry(Some(Verdict::Unknown), Source::AiCampaign, "");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "x",
                false
            ) == Bucket::UnknownVerdict
        );
    }

    // K2: fetch errors surface as FETCH_ERROR (after dispositions, before all live-state buckets).
    #[test]
    fn bucket_fetch_error_masks_state() {
        assert!(
            bucket_of(None, None, Merge::Mergeable, Ci::Green, true, "x", true)
                == Bucket::FetchError
        );
    }

    // K3..K7: live-state precedence chain — CHANGES_REQUESTED > draft > red > conflicting > pending.
    #[test]
    fn bucket_state_precedence_chain() {
        assert!(
            bucket_of(
                None,
                Some("CHANGES_REQUESTED"),
                Merge::Mergeable,
                Ci::Green,
                true,
                "x",
                false
            ) == Bucket::Reject
        );
        assert!(
            bucket_of(None, None, Merge::Mergeable, Ci::Red, true, "x", false) == Bucket::Draft
        );
        assert!(
            bucket_of(None, None, Merge::Conflicting, Ci::Red, false, "x", false)
                == Bucket::ProducerFix
        );
        assert!(
            bucket_of(
                None,
                None,
                Merge::Conflicting,
                Ci::Pending,
                false,
                "x",
                false
            ) == Bucket::Conflicting
        );
        assert!(
            bucket_of(None, None, Merge::Mergeable, Ci::Pending, false, "x", false)
                == Bucket::Pending
        );
    }

    // K8/K9/K15: human-ready or GitHub-approved goes APPROVED; the approval overlay outranks AiVet.
    #[test]
    fn bucket_approved_paths() {
        let e = ventry(Some(Verdict::Ready), Source::Human, "head1");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "head1",
                false
            ) == Bucket::Approved
        );
        assert!(
            bucket_of(
                None,
                Some("APPROVED"),
                Merge::Mergeable,
                Ci::Green,
                false,
                "head1",
                false
            ) == Bucket::Approved
        );
        let ai = ventry(Some(Verdict::Ready), Source::AiCampaign, "head1");
        assert!(
            bucket_of(
                Some(&ai),
                Some("APPROVED"),
                Merge::Mergeable,
                Ci::Green,
                false,
                "head1",
                false
            ) == Bucket::Approved
        );
    }

    // K10: ai-ready on the CURRENT head is AIVET; NoChecks counts like green for this branch.
    #[test]
    fn bucket_aivet_current_head() {
        let e = ventry(Some(Verdict::Ready), Source::AiCampaign, "head1");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "head1",
                false
            ) == Bucket::AiVet
        );
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::NoChecks,
                false,
                "head1",
                false
            ) == Bucket::AiVet
        );
    }

    // K14: a verdict sha that can't be confirmed equal to the live head is STALE — explicit
    // mismatch AND unknown/missing head both count; an empty or "-" verdict sha never does.
    #[test]
    fn bucket_stale_vet_semantics() {
        let e = ventry(Some(Verdict::Ready), Source::AiCampaign, "oldsha");
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "newsha",
                false
            ) == Bucket::StaleVet
        );
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "-",
                false
            ) == Bucket::StaleVet
        );
        assert!(
            bucket_of(
                Some(&e),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "",
                false
            ) == Bucket::StaleVet
        );
        let nosha = ventry(Some(Verdict::Ready), Source::AiCampaign, "");
        assert!(
            bucket_of(
                Some(&nosha),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "anything",
                false
            ) == Bucket::AiVet
        );
        let human = ventry(Some(Verdict::Ready), Source::Human, "oldsha");
        assert!(
            bucket_of(
                Some(&human),
                None,
                Merge::Mergeable,
                Ci::Green,
                false,
                "newsha",
                false
            ) == Bucket::StaleVet
        );
    }

    // K11/K12: no verdict on a green mergeable PR is UNREVIEWED; unresolved mergeability is PENDING.
    #[test]
    fn bucket_unreviewed_and_unknown_mergeable() {
        assert!(
            bucket_of(None, None, Merge::Mergeable, Ci::Green, false, "x", false)
                == Bucket::Unreviewed
        );
        assert!(
            bucket_of(None, None, Merge::Unknown, Ci::Green, false, "x", false) == Bucket::Pending
        );
    }

    // G: cfg precedence — non-empty env > non-empty cron > default (empty values fall through).
    #[test]
    fn cfg_precedence_and_empty_fallthrough() {
        let mut cron = HashMap::new();
        cron.insert("PRR_TEST_K1".to_string(), "from-cron".to_string());
        cron.insert("PRR_TEST_K2".to_string(), "".to_string());
        assert_eq!(cfg(&cron, "PRR_TEST_K1", "def"), "from-cron");
        assert_eq!(
            cfg(&cron, "PRR_TEST_K2", "def"),
            "def",
            "empty cron value must fall to default"
        );
        assert_eq!(cfg(&cron, "PRR_TEST_MISSING", "def"), "def");
        std::env::set_var("PRR_TEST_K3", "from-env");
        let mut c3 = HashMap::new();
        c3.insert("PRR_TEST_K3".to_string(), "from-cron".to_string());
        assert_eq!(cfg(&c3, "PRR_TEST_K3", "def"), "from-env");
        std::env::set_var("PRR_TEST_K4", "");
        let mut c4 = HashMap::new();
        c4.insert("PRR_TEST_K4".to_string(), "from-cron".to_string());
        assert_eq!(
            cfg(&c4, "PRR_TEST_K4", "def"),
            "from-cron",
            "empty env must fall to cron"
        );
    }

    // E: cron.env parsing — comments, export prefix, quotes, trailing comments, HOME expansion.
    #[test]
    fn cron_env_parsing() {
        let home = std::env::var("HOME").unwrap_or_default();
        let m = parse_cron_env(concat!(
            "# comment\n",
            "\n",
            "export A=plain\n",
            "B=\"double quoted # not a comment\"\n",
            "C='single quoted'\n",
            "D=value # trailing comment\n",
            "E=$HOME/sub\n",
            "F=~/tilde\n",
            " G = spaced\n",
            "no_equals_line\n"
        ));
        assert_eq!(m.get("A").unwrap(), "plain");
        assert_eq!(m.get("B").unwrap(), "double quoted # not a comment");
        assert_eq!(m.get("C").unwrap(), "single quoted");
        assert_eq!(m.get("D").unwrap(), "value");
        assert_eq!(m.get("E").unwrap(), &format!("{home}/sub"));
        assert_eq!(m.get("F").unwrap(), &format!("{home}/tilde"));
        assert_eq!(m.get("G").unwrap(), "spaced");
        assert!(!m.contains_key("no_equals_line"));
        assert_eq!(m.len(), 7);
    }

    // S: sanitize replaces tab/newline/cr with spaces and truncates by CODEPOINTS (multibyte-safe).
    #[test]
    fn sanitize_codepoint_truncation() {
        assert_eq!(sanitize("a\tb\nc\rd", 10), "a b c d");
        assert_eq!(sanitize("ééééé", 3), "ééé");
        assert_eq!(sanitize("short", 100), "short");
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
        cost_sidecar_line, has_human_override, labels_to_remove, last_vetter_comment,
        parse_sidecar, should_skip_comment, verdict_comment, verdict_label, verdict_plan,
        VerdictPlan,
    };
    use serde_json::json;

    #[test]
    fn verdict_label_includes_relink() {
        assert_eq!(verdict_label("relink"), Some("ai:relink"));
    }

    // The cost line must be exactly what the queue's parse_sidecar reads back (cheapest-first depends
    // on it once the vetter writes labels, not a ledger).
    #[test]
    fn cost_sidecar_line_round_trips_through_parse_sidecar() {
        let line = cost_sidecar_line("rain.flare", "170", 115, "path refactor", "abc");
        let m = parse_sidecar(&line);
        assert_eq!(
            m.get("rain.flare/170"),
            Some(&(115_i64, "path refactor".to_string(), "abc".to_string()))
        );
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
            verdict_comment("abc123", "ready", "looks good"),
            "🤖 ai:vetter\nReviewed abc123: ready — looks good"
        );
        assert_eq!(
            verdict_comment("abc123", "reject", "   "),
            "🤖 ai:vetter\nReviewed abc123: reject"
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
        let pr = json!({"comments":[
            {"body":"🤖 ai:vetter\nReviewed s1: reject — old"},
            {"body":"a human chiming in"},
            {"body":"🤖 ai:vetter\nReviewed s2: ready — new"}
        ]});
        assert_eq!(
            last_vetter_comment(&pr).as_deref(),
            Some("🤖 ai:vetter\nReviewed s2: ready — new")
        );
        // no vetter comments → None (a non-vetter comment must not match)
        let none = json!({"comments":[{"body":"just a note"}]});
        assert_eq!(last_vetter_comment(&none), None);
    }

    #[test]
    fn human_override_guards_the_verdict() {
        let human = json!({"labels":[{"name":"ai:ready"},{"name":"human:reject"}]});
        assert!(has_human_override(&human), "human:reject must guard");
        let ai_only = json!({"labels":[{"name":"ai:ready"}]});
        assert!(!has_human_override(&ai_only));
    }
}
