//! Behavioural tests for `run-timings`: the filter that records `bootMs` and `ttlMs` from a LIVE
//! trace stream instead of at run end.
//!
//! The whole point of the subcommand is a property no unit test can demonstrate: that the numbers
//! are on disk BEFORE the process that produced them is gone. `run-metrics` runs after claude
//! exits, so a run that is killed or times out records nothing — three manual vetter runs on
//! 2026-07-27/28 produced zero records between them, and the 5-minute time-to-first-verdict they
//! were opened to measure had to be read off the trace by hand (#84).
//!
//! So these drive the REAL binary as a real child process, feed it a real event stream on stdin,
//! and — in the test that matters — SIGKILL it mid-stream and then read the file. A stub would
//! have asserted our own belief about when a write lands.
//!
//! These live in `tests/` rather than in `src/main.rs`'s `#[cfg(test)]` module because they need
//! no access to the binary's internals, only its behaviour as a process.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

/// The binary cargo just built for this test. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// A temp dir that removes itself. `std::env::temp_dir` plus the test's own name keeps concurrent
/// tests (cargo runs them threaded) out of each other's files.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("prr-run-timings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
    fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The five load-bearing events of the live vetter run `review-runs/20260728T053610Z.jsonl`:
/// opening text, first tool call, its result, first verdict, the verdict's result. Every timestamp
/// is the real one, so the numbers these tests assert are the numbers that run actually produced.
fn events() -> Vec<String> {
    vec![
        r#"{"type":"assistant","timestamp":"2026-07-28T05:36:13.214Z","message":{"content":[{"type":"text","text":"starting"}]}}"#.into(),
        r#"{"type":"assistant","timestamp":"2026-07-28T05:36:14.339Z","message":{"content":[{"type":"tool_use","name":"mcp__fsm__unvetted","input":{}}]}}"#.into(),
        r#"{"type":"user","timestamp":"2026-07-28T05:38:30.435Z","message":{"content":[]}}"#.into(),
        r#"{"type":"assistant","timestamp":"2026-07-28T05:41:11.355Z","message":{"content":[{"type":"tool_use","name":"mcp__fsm__record_verdict","input":{}}]}}"#.into(),
        r#"{"type":"user","timestamp":"2026-07-28T05:41:16.994Z","message":{"content":[]}}"#.into(),
    ]
}

fn spawn(out: &std::path::Path) -> Child {
    Command::new(BIN)
        .args([
            "run-timings",
            "--out",
            out.to_str().unwrap(),
            "--trace",
            "/runs/20260728T053610Z.jsonl",
            "--run-id",
            "20260728T053610Z",
            "--role",
            "vetter",
            "--model",
            "claude-fable-5",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run-timings")
}

fn records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each appended line is one JSON object"))
        .collect()
}

/// Wait until the metrics file holds `n` records, or give up. Polling beats a fixed sleep: it
/// makes "the write landed" the condition rather than a guess about how fast the box is.
fn wait_for(path: &std::path::Path, n: usize) -> Vec<serde_json::Value> {
    for _ in 0..600 {
        let r = records(path);
        if r.len() >= n {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    records(path)
}

#[test]
fn a_completed_stream_records_boot_then_ttl() {
    let dir = TempDir::new("complete");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    {
        let mut si = child.stdin.take().expect("stdin");
        for e in events() {
            writeln!(si, "{e}").unwrap();
        }
    } // dropping stdin closes it, ending the stream
    let echoed = {
        let so = child.stdout.take().expect("stdout");
        BufReader::new(so)
            .lines()
            .map(|l| l.unwrap())
            .collect::<Vec<_>>()
    };
    assert!(child.wait().unwrap().success());

    // Pass-through first: the runner pipes this into the distiller, so every byte must survive.
    assert_eq!(echoed, events(), "the stream must pass through unchanged");

    let recs = records(&out);
    assert_eq!(
        recs.iter()
            .map(|r| r["stage"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["boot", "ttl"],
        "one record per number, in the order the numbers became knowable"
    );
    assert_eq!(recs[0]["bootMs"], 1125);
    assert_eq!(recs[1]["bootMs"], 1125);
    assert_eq!(recs[1]["ttlMs"], 297_016);
    for r in &recs {
        assert_eq!(r["runId"], "20260728T053610Z");
        assert_eq!(r["role"], "vetter");
        assert_eq!(r["model"], "claude-fable-5");
        assert_eq!(r["trace"], "/runs/20260728T053610Z.jsonl");
    }
    // The directory did not exist when the run started: a missing metrics/ must not lose the run.
    assert!(out.exists());
}

/// THE test. A run that is killed is exactly the run whose startup timings you want, and it is the
/// one `run-metrics` can never reach. Both numbers must already be on disk when the process dies.
#[test]
fn a_killed_run_keeps_the_numbers_it_already_knew() {
    let dir = TempDir::new("killed");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut si = child.stdin.take().expect("stdin");
    for e in events() {
        writeln!(si, "{e}").unwrap();
    }
    si.flush().unwrap();
    // stdin stays OPEN: the stream has not ended, the run is still "going", and nothing downstream
    // has run. This is the state a killed cron run is in.
    let recs = wait_for(&out, 2);
    child.kill().expect("kill the run mid-stream");
    let status = child.wait().unwrap();
    assert!(!status.success(), "the run was killed, not completed");

    let after = records(&out);
    assert_eq!(
        after.len(),
        2,
        "a killed run still contributes both numbers"
    );
    assert_eq!(after, recs);
    assert_eq!(after[0]["stage"], "boot");
    assert_eq!(after[0]["bootMs"], 1125);
    assert_eq!(after[1]["stage"], "ttl");
    assert_eq!(after[1]["ttlMs"], 297_016);
    // …and nothing a killed run cannot know.
    for k in [
        "toolCalls",
        "startupPct",
        "durationMs",
        "outcome",
        "exitCode",
    ] {
        assert!(
            after.iter().all(|r| r.get(k).is_none()),
            "a killed run must not report {k}"
        );
    }
}

/// A run killed BEFORE its first productive call still contributes boot — the launch cost is
/// knowable long before the model has done anything with it.
#[test]
fn a_run_killed_before_its_first_verdict_still_contributes_boot() {
    let dir = TempDir::new("early");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut si = child.stdin.take().expect("stdin");
    for e in events().into_iter().take(2) {
        writeln!(si, "{e}").unwrap();
    }
    si.flush().unwrap();
    let recs = wait_for(&out, 1);
    child.kill().expect("kill");
    let _ = child.wait();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["stage"], "boot");
    assert_eq!(recs[0]["bootMs"], 1125);
    assert!(recs[0].get("ttlMs").is_none());
}

/// Garbage on the wire is not this filter's problem to have an opinion about: it must forward
/// every line and keep measuring across it. A run whose trace has one corrupt line still gets its
/// timings, and the distiller downstream still gets its whole stream.
#[test]
fn unparseable_lines_pass_through_and_do_not_stop_the_measurement() {
    let dir = TempDir::new("garbage");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut sent = Vec::new();
    {
        let mut si = child.stdin.take().expect("stdin");
        for (i, e) in events().into_iter().enumerate() {
            if i == 1 {
                writeln!(si, "{{not json").unwrap();
                sent.push("{not json".to_string());
            }
            writeln!(si, "{e}").unwrap();
            sent.push(e);
        }
    }
    let echoed = {
        let so = child.stdout.take().expect("stdout");
        BufReader::new(so)
            .lines()
            .map(|l| l.unwrap())
            .collect::<Vec<_>>()
    };
    assert!(child.wait().unwrap().success());
    assert_eq!(echoed, sent, "even a corrupt line is forwarded verbatim");
    let recs = records(&out);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["bootMs"], 1125);
    assert_eq!(recs[1]["ttlMs"], 297_016);
}
