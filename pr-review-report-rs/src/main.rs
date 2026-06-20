// pr-review-report — report every open PR (and logged close-candidate) that needs a HUMAN decision,
// RESPECTING reviews already done: it overlays (a) recorded review verdicts in review-verdicts.jsonl
// and (b) GitHub's own review state (APPROVED / CHANGES_REQUESTED) on top of the CI/mergeability
// signal. Rust rewrite of pr-review-report.sh, fixing the 16 bugs from the adversarial review.
//
// Usage:   pr-review-report            # all buckets
//          pr-review-report --ready    # only the reviewed-&-ready-to-merge bucket
// Config (env overrides cron.env in CWD, then default): ORG, PR_ASSIGNEE, CLOSE_CANDIDATES, REVIEW_VERDICTS.

use serde_json::Value;
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq)]
enum Ci { Red, Pending, NoChecks, Green }

#[derive(Clone, Copy, PartialEq)]
enum Merge { Mergeable, Conflicting, Unknown }

#[derive(Clone, Copy, PartialEq)]
enum Verdict { Ready, Relink, Reject, Close, Unknown }

#[derive(Clone, Copy, PartialEq)]
enum Source { Human, AiCampaign, Other }

#[derive(Clone, Copy, PartialEq)]
enum Bucket {
    Approved, AiVet, StaleVet, ProducerFix, Relink, Reject, Close,
    Unreviewed, Conflicting, Pending, Draft, FetchError, UnknownVerdict,
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
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).take(n).collect()
}

/// Run gh and parse stdout as JSON; None on non-zero exit, spawn failure, or unparseable output.
fn gh_json(args: &[&str]) -> Option<Value> {
    let out = Command::new("gh").args(args).output().ok()?;
    if !out.status.success() { return None; }
    serde_json::from_slice(&out.stdout).ok()
}

struct VEntry { verdict: Option<Verdict>, source: Source, sha: String, note: String }

/// FIX(bug 1,6,7): parse the ledger line-by-line so one malformed line (e.g. `hello`) can't drop
/// every verdict after it; normalize (trim+lowercase) the verdict; last matching line wins per key.
fn load_verdicts(path: &str) -> HashMap<String, VEntry> {
    let mut m = HashMap::new();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    for line in content.lines() {
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        if !v.is_object() { continue; }
        let repo = match v.get("repo").and_then(|x| x.as_str()) { Some(r) => r, None => continue };
        let pr = match v.get("pr") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let key = format!("{repo}/{pr}");
        let raw = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("").trim().to_lowercase();
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
        let sha = v.get("sha").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let note = sanitize(v.get("note").and_then(|x| x.as_str()).unwrap_or(""), 100);
        m.insert(key, VEntry { verdict, source, sha, note });
    }
    m
}

struct PrRow {
    repo: String, num: u64, merge: Merge, ci: Ci, draft: bool,
    rev: Option<String>, url: String, headoid: String, title: String, fetch_error: bool,
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
        let is_fail = matches!(concl, Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED") | Some("STARTUP_FAILURE"))
            || matches!(state, Some("FAILURE") | Some("ERROR"));
        if is_fail { fail += 1; continue; }
        let is_pend = if let Some(st) = status {
            st != "COMPLETED"
        } else if let Some(s) = state {
            !matches!(s, "SUCCESS" | "FAILURE" | "ERROR")
        } else {
            // FIX(rs-bug 3): a check with neither status nor state is unconfirmed → pending, never green.
            true
        };
        if is_pend { pend += 1; }
    }
    if fail > 0 { Ci::Red } else if pend > 0 { Ci::Pending } else if tot == 0 { Ci::NoChecks } else { Ci::Green }
}

/// FIX(bug 3,4,5): on gh failure, flag fetch_error (→ a visible FETCH_ERROR bucket) instead of
/// emitting a `?` row that silently becomes PENDING. URL falls back to the constructed PR url.
fn classify_one(org: &str, repo: &str, num: u64) -> PrRow {
    let target = format!("{org}/{repo}");
    let url_fb = format!("https://github.com/{org}/{repo}/pull/{num}");
    let j = gh_json(&[
        "pr", "view", &num.to_string(), "-R", &target, "--json",
        "url,mergeable,isDraft,reviewDecision,statusCheckRollup,headRefOid,title",
    ]);
    match j {
        None => PrRow {
            repo: repo.to_string(), num, merge: Merge::Unknown, ci: Ci::NoChecks, draft: false,
            rev: None, url: url_fb, headoid: "-".to_string(), title: String::new(), fetch_error: true,
        },
        Some(j) => {
            let url = j.get("url").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).unwrap_or(&url_fb).to_string();
            let merge = match j.get("mergeable").and_then(|x| x.as_str()) {
                Some("MERGEABLE") => Merge::Mergeable,
                Some("CONFLICTING") => Merge::Conflicting,
                _ => Merge::Unknown,
            };
            let draft = j.get("isDraft").and_then(|x| x.as_bool()).unwrap_or(false);
            let rev = j.get("reviewDecision").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
            let ci = classify_ci(j.get("statusCheckRollup").unwrap_or(&Value::Null));
            let headoid = j.get("headRefOid").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).unwrap_or("-").to_string();
            let title = sanitize(j.get("title").and_then(|x| x.as_str()).unwrap_or(""), 100);
            PrRow { repo: repo.to_string(), num, merge, ci, draft, rev, url, headoid, title, fetch_error: false }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn bucket_of(e: Option<&VEntry>, rev: Option<&str>, merge: Merge, ci: Ci, draft: bool, headoid: &str, fetch_error: bool) -> Bucket {
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
    if fetch_error { return Bucket::FetchError; }
    if rev == Some("CHANGES_REQUESTED") { return Bucket::Reject; }
    if draft { return Bucket::Draft; }
    if ci == Ci::Red { return Bucket::ProducerFix; }
    if merge == Merge::Conflicting { return Bucket::Conflicting; }
    if ci == Ci::Pending { return Bucket::Pending; }
    let is_ready = verdict == Some(Verdict::Ready);
    let src = e.map(|x| x.source);
    let approved = rev == Some("APPROVED") || (is_ready && src == Some(Source::Human));
    let aivet = !approved && is_ready && src == Some(Source::AiCampaign);
    if merge == Merge::Mergeable && (ci == Ci::Green || ci == Ci::NoChecks) {
        let vsha = e.map(|x| x.sha.as_str()).unwrap_or("");
        // FIX(rs-bug 2): a recorded verdict with a real sha is STALE whenever the live head can't be
        // confirmed equal — including a missing/unknown head ("-") — not only on an explicit mismatch.
        if (approved || aivet) && !vsha.is_empty() && vsha != "-" && (headoid.is_empty() || headoid == "-" || vsha != headoid) {
            return Bucket::StaleVet;
        }
        if approved { return Bucket::Approved; }
        if aivet { return Bucket::AiVet; }
        return Bucket::Unreviewed;
    }
    Bucket::Pending // mergeability still resolving (mergeable=UNKNOWN)
}

struct OutRow { bucket: Bucket, url: String, oneliner: String }

fn cfg(cron: &HashMap<String, String>, env_key: &str, def: &str) -> String {
    // FIX(rs-bug 5): a set-but-EMPTY env OR cron.env value falls back to the default (bash ${VAR:-def}).
    std::env::var(env_key).ok().filter(|s| !s.is_empty())
        .or_else(|| cron.get(env_key).cloned().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| def.to_string())
}

/// FIX(rs-bug 6): resolve the base dir (where the ledgers live) like bash's $DIR, so the binary finds
/// them regardless of CWD: RAINIX_BATCH_DIR env, else walk up from the exe to the first dir holding a
/// ledger / cron.env, else ".".
fn base_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("RAINIX_BATCH_DIR") {
        if !d.is_empty() { return std::path::PathBuf::from(d); }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..6 {
            let Some(d) = dir else { break };
            if d.join("review-verdicts.jsonl").exists() || d.join("cron.env").exists() || d.join("close-candidates.jsonl").exists() {
                return d;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    std::path::PathBuf::from(".")
}

fn main() {
    let base = base_dir();
    // Parse cron.env (KEY=VALUE) from the base dir if present; env vars override it; then defaults.
    let mut cron: HashMap<String, String> = HashMap::new();
    if let Ok(c) = std::fs::read_to_string(base.join("cron.env")) {
        for line in c.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((k, v0)) = line.split_once('=') {
                let v0 = v0.trim_start();
                // FIX(rs-bug 7): honour quotes; strip a trailing unquoted ` #...` comment; expand $HOME/~.
                let val = if let Some(rest) = v0.strip_prefix('"') {
                    rest.split_once('"').map(|(inner, _)| inner.to_string()).unwrap_or_else(|| rest.to_string())
                } else if let Some(rest) = v0.strip_prefix('\'') {
                    rest.split_once('\'').map(|(inner, _)| inner.to_string()).unwrap_or_else(|| rest.to_string())
                } else {
                    let cut = v0.find(" #").or_else(|| v0.find("\t#")).unwrap_or(v0.len());
                    v0[..cut].trim().to_string()
                };
                let val = if let Some(rest) = val.strip_prefix("$HOME") {
                    format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)
                } else if let Some(rest) = val.strip_prefix("~/") {
                    format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
                } else { val };
                cron.insert(k.trim().to_string(), val);
            }
        }
    }
    let org = cfg(&cron, "ORG", "rainlanguage");
    // FIX(rs-bug 5): PR_ASSIGNEE via cfg() so a set-but-empty value also falls back to the default.
    let author = cfg(&cron, "PR_ASSIGNEE", "thedavidmeister");
    // FIX(rs-bug 6): default the ledgers to the base dir (found regardless of CWD).
    let cc_def = base.join("close-candidates.jsonl").to_string_lossy().into_owned();
    let rv_def = base.join("review-verdicts.jsonl").to_string_lossy().into_owned();
    let close_candidates = cfg(&cron, "CLOSE_CANDIDATES", &cc_def);
    let review_verdicts = cfg(&cron, "REVIEW_VERDICTS", &rv_def);
    let only_ready = std::env::args().nth(1).as_deref() == Some("--ready");

    let now = Command::new("date").args(["-u", "+%FT%TZ"]).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("PR review report — {}, author {} — {}", org, author, now);
    println!("pipeline:  🟦 unreviewed  →  🤖 AI-vetted  →  ✅ you approve  →  merge");
    println!("(AI verdicts: review-verdicts.jsonl · your sign-off: a GitHub approval, or a verdict with source=human)");
    println!("================================================================");

    // FIX(bug 4): a failed `gh search prs` aborts loudly — never a falsely-empty all-clear.
    let search = Command::new("gh")
        .args(["search", "prs", "--owner", &org, "--author", &author, "--state", "open", "--limit", "300", "--json", "repository,number"])
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
    let prs: Vec<(String, u64)> = arr.iter().filter_map(|x| {
        let repo = x.get("repository").and_then(|r| r.get("name")).and_then(|n| n.as_str())?.to_string();
        let num = x.get("number").and_then(|n| n.as_u64())?;
        Some((repo, num))
    }).collect();

    let verds = load_verdicts(&review_verdicts);

    // Bounded-concurrency per-PR fan-out (~12 workers, std only) via scoped threads + an atomic cursor.
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<PrRow>> = Mutex::new(Vec::with_capacity(prs.len()));
    let nworkers = std::cmp::min(12, std::cmp::max(1, prs.len()));
    std::thread::scope(|s| {
        for _ in 0..nworkers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= prs.len() { break; }
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
        let b = bucket_of(e, r.rev.as_deref(), r.merge, r.ci, r.draft, &r.headoid, r.fetch_error);
        let note = e.map(|x| x.note.clone()).unwrap_or_default();
        let oneliner = if note.is_empty() || note == "approved by user" {
            if r.title.is_empty() {
                if r.fetch_error { "(gh pr view failed — state unknown)".to_string() } else { "?".to_string() }
            } else {
                r.title.clone()
            }
        } else {
            note
        };
        outs.push(OutRow { bucket: b, url: r.url.clone(), oneliner });
    }

    let emit = |bucket: Bucket, header: &str| {
        let mut lines: Vec<String> = outs.iter()
            .filter(|o| o.bucket == bucket)
            .map(|o| format!("  {}  —  {}", o.url, o.oneliner))
            .collect();
        if lines.is_empty() { return; }
        lines.sort();
        println!();
        println!("{}  ({})", header, lines.len());
        for l in &lines { println!("{}", l); }
    };

    emit(Bucket::Approved, "✅ APPROVED BY YOU — ready to merge (GitHub approval / verdict you set)");
    if only_ready { return; }
    emit(Bucket::AiVet, "🤖 AI-VETTED — awaiting YOUR approval (passed the automated review; NOT human-reviewed yet)");
    emit(Bucket::StaleVet, "🔄 RE-VET PENDING — head moved since the recorded verdict (e.g. a producer step-3b fix); the vetter re-reviews the new commit before this can merge");
    emit(Bucket::ProducerFix, "🔴 RED — NEEDS A PRODUCER FIX (CI failing; the producer cron diagnoses it and pushes a fix to drive it green — producer work, NOT 'blocked', NOT your action)");
    emit(Bucket::Relink, "🔧 AI-flagged — relink Closes→Refs before merge (else it auto-closes a live issue)");
    emit(Bucket::Reject, "❌ AI-flagged / you requested changes — rework or close");
    emit(Bucket::Close, "🗑️  AI-flagged — close (duplicate / superseded)");
    emit(Bucket::Unreviewed, "🟦 NOT YET REVIEWED — green + mergeable, awaiting AI review + your approval");
    emit(Bucket::Conflicting, "⚠️  CONFLICTING — needs a rebase onto current main (producer work)");
    emit(Bucket::Pending, "🟡 PENDING — CI / mergeability still resolving (no action; just wait)");
    emit(Bucket::Draft, "📝 DRAFTS — intentionally not ready");
    // FIX(bug 3,5): degraded/partial data is visible, not masked as a settled PENDING.
    emit(Bucket::FetchError, "⚠️  COULD NOT FETCH — gh pr view failed; state UNKNOWN (re-run; NOT a settled 'just wait')");
    emit(Bucket::UnknownVerdict, "❓ UNKNOWN VERDICT — ledger has a non-canonical verdict (expected ready|relink|reject|close); fix the ledger line");

    close_candidates_section(&org, &close_candidates);

    println!();
    println!("----------------------------------------------------------------");
    println!("totals: {} open PRs by {}  ·  buckets:", rows.len(), author);
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for o in &outs { *counts.entry(o.bucket.key()).or_insert(0) += 1; }
    let mut hv: Vec<(&str, usize)> = counts.into_iter().collect();
    hv.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (k, c) in hv { println!("   {} {}", c, k); }
}

/// FIX(bug 1,8,9,10,11 + rs-bug 8): robust line-by-line parse; require a string repo; dedup on the
/// (repo,issue) identity (not the url, so an explicit url vs the constructed fallback for the same
/// issue can't double-count); classify each reason as NOT-LANDED (covered/made-moot by an OPEN pr →
/// exclude + counted separately), LANDED (show), or UNRECOGNIZED (show, tagged — never silently
/// dropped); FAIL-OPEN on the live state check (gh error → state UNKNOWN, still SHOWN).
fn close_candidates_section(org: &str, path: &str) {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    if content.trim().is_empty() { return; }
    let mut latest: HashMap<String, (String, String, String, String)> = HashMap::new(); // id -> (full, issue, url, reason)
    for line in content.lines() {
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        if !v.is_object() { continue; }
        if v.get("issue").map(|x| x.is_null()).unwrap_or(true) { continue; }
        let issue = match v.get("issue") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let repo = match v.get("repo").and_then(|x| x.as_str()) { Some(r) => r.to_string(), None => continue };
        let full = if repo.contains('/') { repo } else { format!("{org}/{repo}") };
        let url = v.get("url").and_then(|x| x.as_str()).map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://github.com/{full}/issues/{issue}"));
        let reason = sanitize(
            v.get("reason").and_then(|x| x.as_str())
                .or_else(|| v.get("note").and_then(|x| x.as_str()))
                .unwrap_or(""), 80);
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
        let not_landed = rl.contains("open-pr") || rl.contains("open pr") || rl.contains("covered-by-open")
            || rl.contains("made-moot-by") || rl.contains("opened to") || rl.contains("pr opened") || rl.contains("opened a pr");
        if not_landed { not_landed_c += 1; continue; }
        // LANDED: the leading token is a canonical landed reason, OR a won't-fix token appears anywhere.
        let lead = rl.split(|c: char| c == ' ' || c == ':').next().unwrap_or("");
        let landed = matches!(lead, "already-fixed-on-main" | "already-addressed-by-ci" | "invalid" | "duplicate" | "wont-fix" | "won't-fix" | "wontfix")
            || rl.contains("wontfix") || rl.contains("won't-fix") || rl.contains("wont-fix") || rl.contains("won't fix") || rl.contains("wont fix");
        // UNRECOGNIZED (neither not-landed nor landed) is still SHOWN, tagged — never silently dropped.
        let tag = if landed { "" } else { "  [reason unrecognized — verify]" };
        let st = gh_json(&["issue", "view", &issue, "-R", &full, "--json", "state"])
            .and_then(|j| j.get("state").and_then(|x| x.as_str()).map(|s| s.to_string()));
        match st.as_deref() {
            Some("OPEN") => { open_c += 1; rows.push(format!("  {}  — {}{}", url, reason, tag)); }
            Some("CLOSED") => { closed_c += 1; }
            _ => { unknown_c += 1; rows.push(format!("  {}  — {} (state UNKNOWN — gh error){}", url, reason, tag)); }
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
        for r in &rows { println!("{}", r); }
    }
}
