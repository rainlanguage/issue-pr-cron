//! Behavioural tests for the LIVE token-usage and rate-limit records `run-timings` writes (#97).
//!
//! `run-metrics` reads tokens from the terminal `result` event, so a killed run reports zeros —
//! and a killed run is the one whose spend you wanted to see. These drive the real binary as a
//! real child process, feed it a real event stream, and in the test that matters SIGKILL it
//! mid-stream and then read the file. The claim is about what is on disk when the process is
//! gone, so nothing here may be a stub.
//!
//! The stream is the real vetter run `review-runs/20260728T100257Z.jsonl`, reduced to the fields
//! the probes read. Its 118 `assistant` events carry 37 unique message ids — the duplication that
//! makes a naive sum wrong is present, and the totals asserted are the ones its own `result`
//! event reported.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

/// The binary cargo just built for this test. Never a PATH lookup — that would test whatever
/// `pr-review-report` happens to be installed on the box.
const BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

const VETTER_FIXTURE: &str = include_str!("fixtures/vetter-20260728T100257Z.usage.jsonl");

/// The authoritative totals from that run's own `result` event.
const TRUE_TOKENS_IN: u64 = 74;
const TRUE_CACHE_READ: u64 = 6_099_441;
const TRUE_CACHE_CREATION: u64 = 303_723;
const TRUE_MESSAGES: u64 = 37;

/// The two wrong answers #97 warns about, reachable from this very stream. Asserting the records
/// are NEITHER is what makes these tests discriminating rather than self-confirming.
const NAIVE_PER_EVENT_CACHE_READ: u64 = 18_771_942;

/// Mirrors `USAGE_RECORD_STRIDE` in the binary, which these tests cannot import. The unit test
/// `the_usage_record_stride_is_bounded_and_nonzero` pins the real constant, so a drift between the
/// two fails there rather than turning these into a silent no-op.
const USAGE_RECORD_STRIDE: u64 = 25;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("prr-run-usage-{tag}-{}", std::process::id()));
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

fn events() -> Vec<String> {
    VETTER_FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn spawn(out: &std::path::Path) -> Child {
    Command::new(BIN)
        .args([
            "run-timings",
            "--out",
            out.to_str().unwrap(),
            "--trace",
            "/review-runs/20260728T100257Z.jsonl",
            "--run-id",
            "20260728T100257Z",
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

fn usage_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    records(path)
        .into_iter()
        .filter(|r| r["stage"] == "usage")
        .collect()
}

/// Wait until at least `n` usage records exist, or give up. Polling makes "the write landed" the
/// condition rather than a guess about how fast the box is.
fn wait_for_usage(path: &std::path::Path, n: usize) -> Vec<serde_json::Value> {
    wait_until(path, |r| r.len() >= n)
}

/// Wait until a usage record reports at least `n` messages.
///
/// NOT the same condition as "n records exist". The very first `rate_limit_event` escalates a
/// window from unrecorded to recorded and writes a record right there — legitimately reporting
/// zero spend, because at that instant none had been incurred. In the real vetter trace that
/// event is the FIRST line, so "one record exists" is satisfied before a single message has been
/// counted. Waiting on record COUNT therefore raced: it passed on Linux and failed on macOS,
/// asserting against that zero-spend record. Waiting on the quantity under test is the fix.
fn wait_for_messages(path: &std::path::Path, n: u64) -> Vec<serde_json::Value> {
    wait_until(path, |r| {
        r.iter().any(|x| x["messages"].as_u64().unwrap_or(0) >= n)
    })
}

fn wait_until(
    path: &std::path::Path,
    done: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    for _ in 0..600 {
        let r = usage_records(path);
        if done(&r) {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    usage_records(path)
}

/// A whole stream, run to completion: the last usage record must equal the `result` event's own
/// totals, and must not equal either plausible wrong answer.
#[test]
fn a_completed_stream_ends_on_the_authoritative_totals() {
    let dir = TempDir::new("complete");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    {
        let mut si = child.stdin.take().expect("stdin");
        for e in events() {
            writeln!(si, "{e}").unwrap();
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
    // Pass-through first: the runner pipes this into the distiller, so every byte must survive.
    assert_eq!(echoed, events(), "the stream must pass through unchanged");

    let us = usage_records(&out);
    assert!(!us.is_empty(), "a run must record its usage");
    let last = us.last().unwrap();
    assert_eq!(last["tokensIn"], TRUE_TOKENS_IN);
    assert_eq!(last["cacheRead"], TRUE_CACHE_READ);
    assert_eq!(last["cacheCreation"], TRUE_CACHE_CREATION);
    assert_eq!(last["messages"], TRUE_MESSAGES);
    assert_ne!(
        last["cacheRead"], NAIVE_PER_EVENT_CACHE_READ,
        "a per-event sum is the wrong answer"
    );
    // No guessed output count — see the "Live token usage" comment in main.rs.
    assert!(us.iter().all(|r| r.get("tokensOut").is_none()));

    // Identity is stamped on every record, so a line is attributable on its own.
    for r in &us {
        assert_eq!(r["runId"], "20260728T100257Z");
        assert_eq!(r["role"], "vetter");
        assert_eq!(r["model"], "claude-fable-5");
        assert_eq!(r["trace"], "/review-runs/20260728T100257Z.jsonl");
    }
    // The startup records still land: #97 extends #88's mechanism, it does not displace it.
    let all = records(&out);
    let stages: Vec<&str> = all.iter().map(|r| r["stage"].as_str().unwrap()).collect();
    assert!(stages.contains(&"usage"));
    // The totals only ever climb.
    let reads: Vec<u64> = us
        .iter()
        .map(|r| r["cacheRead"].as_u64().unwrap())
        .collect();
    assert!(
        reads.windows(2).all(|w| w[0] <= w[1]),
        "usage records must be monotonic, got {reads:?}"
    );
    assert!(
        us.len() >= 2,
        "37 messages at a stride of 25 gives a mid-run record and a final one, got {}",
        us.len()
    );
}

/// THE test of #97. A run killed mid-flight is exactly the run whose spend you wanted, and it is
/// the one `run-metrics` can never reach — it never reads a `result` event, so it reports zeros.
/// The numbers must already be on disk while the process is still alive, and survive its death.
#[test]
fn a_killed_run_keeps_the_spend_it_had_already_incurred() {
    let dir = TempDir::new("killed");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut si = child.stdin.take().expect("stdin");
    // Feed enough to cross the stride, then STOP — stdin stays open, the run is still "going",
    // and nothing downstream has run. This is the state a killed cron run is in.
    for e in events().iter().take(90) {
        writeln!(si, "{e}").unwrap();
    }
    si.flush().unwrap();
    // Wait for the STRIDE record specifically, not merely for "a record": the first
    // `rate_limit_event` writes one before any message is counted. See `wait_for_messages`.
    let before = wait_for_messages(&out, USAGE_RECORD_STRIDE);
    assert!(
        !before.is_empty(),
        "spend must be on disk BEFORE the process dies"
    );

    child.kill().expect("kill the run mid-stream");
    let status = child.wait().unwrap();
    assert!(!status.success(), "the run was killed, not completed");

    let after = usage_records(&out);
    assert_eq!(after, before, "the file survives the kill unchanged");
    let last = after.last().unwrap();
    // A real, non-zero, partial figure — the whole point. Less than the finished run's total,
    // because the run did not finish.
    let read = last["cacheRead"].as_u64().unwrap();
    assert!(read > 0, "a killed run must not report zero spend");
    assert!(
        read < TRUE_CACHE_READ,
        "a partial run must not report the completed total, got {read}"
    );
    assert!(last["messages"].as_u64().unwrap() >= USAGE_RECORD_STRIDE);
    // …and nothing a killed run cannot know.
    for k in ["toolCalls", "durationMs", "outcome", "exitCode", "costUsd"] {
        assert!(
            after.iter().all(|r| r.get(k).is_none()),
            "a killed run must not report {k}"
        );
    }
}

/// The rate-limit half. Nine `five_hour` events are in this stream; the window must appear on the
/// record — that is the blind spot #97 opened with, where a run that trips the five-hour limit
/// "presents as an unexplained stall with no evidence of why".
#[test]
fn the_five_hour_window_reaches_the_record() {
    let dir = TempDir::new("ratelimit");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    {
        let mut si = child.stdin.take().expect("stdin");
        for e in events() {
            writeln!(si, "{e}").unwrap();
        }
    }
    assert!(child.wait().unwrap().success());
    let last = usage_records(&out).last().cloned().expect("a usage record");
    assert_eq!(last["rateLimits"]["five_hour"]["events"], 9);
    assert_eq!(last["rateLimits"]["five_hour"]["status"], "allowed");
    assert_eq!(
        last["rateLimits"]["five_hour"]["resetsAt"],
        1_785_237_000i64
    );
}

/// An escalation is written the MOMENT it happens, not at the next stride. A run that gets
/// rejected usually stops producing messages straight after, so waiting for 25 more would mean
/// never recording it — which is the failure mode, not a delay in reporting it.
#[test]
fn a_rejection_is_recorded_immediately_not_at_the_next_stride() {
    let dir = TempDir::new("escalation");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut si = child.stdin.take().expect("stdin");
    // One message: far below the stride, so no usage record is due yet.
    writeln!(
        si,
        r#"{{"type":"assistant","message":{{"id":"m1","usage":{{"input_tokens":1,"cache_read_input_tokens":500,"cache_creation_input_tokens":0}}}}}}"#
    )
    .unwrap();
    si.flush().unwrap();
    // The run is refused.
    writeln!(
        si,
        r#"{{"type":"rate_limit_event","rate_limit_info":{{"rateLimitType":"five_hour","status":"rejected","resetsAt":1785237000}}}}"#
    )
    .unwrap();
    si.flush().unwrap();

    let us = wait_for_usage(&out, 1);
    child.kill().expect("kill");
    let _ = child.wait();
    assert_eq!(
        us.len(),
        1,
        "the escalation alone must produce a record, before the stride"
    );
    assert_eq!(us[0]["rateLimits"]["five_hour"]["status"], "rejected");
    assert_eq!(
        us[0]["rateLimits"]["five_hour"]["resetsAt"],
        1_785_237_000i64
    );
    assert_eq!(us[0]["cacheRead"], 500, "and it carries the spend so far");
    assert_eq!(us[0]["messages"], 1);
}

/// The OTHER way this filter's stream ends: the DOWNSTREAM closes. `run-timings` sits in the
/// middle of the runner's pipe (`tee "$RUNLOG" | run-timings | <distiller>`), so if the distiller
/// dies the echo fails with `EPIPE` long before stdin runs out. That exit is not a reason to throw
/// the run's measured spend away — the tokens were still spent — so it must write the same
/// end-of-stream record a clean EOF does.
///
/// Driven deterministically: five messages are echoed into the pipe buffer and read back (so the
/// child has certainly counted them), the read end is then dropped, and one more line is written.
/// The child's echo of that line is what fails. Five is below the stride, so no record is due —
/// the only record that can exist is the one this exit path writes.
#[test]
fn a_closed_downstream_still_records_the_spend_so_far() {
    let dir = TempDir::new("downstream");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut si = child.stdin.take().expect("stdin");
    let so = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(so);

    let msgs: Vec<String> = (0..5)
        .map(|i| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"m{i}","usage":{{"input_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}}}}}}"#
            )
        })
        .collect();
    for m in &msgs {
        writeln!(si, "{m}").unwrap();
    }
    si.flush().unwrap();
    // Read them back: the child cannot have echoed a line it had not yet parsed.
    for _ in 0..msgs.len() {
        let mut line = String::new();
        reader.read_line(&mut line).expect("echo");
        assert!(!line.is_empty());
    }
    drop(reader); // the distiller dies

    // One more line. Echoing it is what hits EPIPE.
    let _ = writeln!(si, "{}", msgs[0].replace("\"m0\"", "\"m9\""));
    let _ = si.flush();
    drop(si);
    let _ = child.wait();

    let us = usage_records(&out);
    assert_eq!(
        us.len(),
        1,
        "a closed downstream must still leave exactly the end-of-stream record"
    );
    assert!(
        us[0]["messages"].as_u64().unwrap() >= 5,
        "it must carry the messages already counted, got {}",
        us[0]["messages"]
    );
    assert!(us[0]["cacheRead"].as_u64().unwrap() >= 500);
}

/// A run that never emits a message writes no usage record at all — an empty stream must not
/// leave a line claiming a measured zero.
#[test]
fn a_stream_with_no_messages_records_no_usage() {
    let dir = TempDir::new("empty");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    {
        let mut si = child.stdin.take().expect("stdin");
        writeln!(si, r#"{{"type":"system","subtype":"init"}}"#).unwrap();
        writeln!(si, r#"{{"type":"user","message":{{"content":[]}}}}"#).unwrap();
    }
    assert!(child.wait().unwrap().success());
    assert!(
        usage_records(&out).is_empty(),
        "no messages means no measured spend"
    );
}

/// Garbage on the wire is not this filter's problem to have an opinion about: it forwards every
/// line and keeps measuring across it. A trace with a corrupt line still gets its usage, and the
/// distiller downstream still gets its whole stream.
#[test]
fn a_corrupt_line_passes_through_and_does_not_stop_the_measurement() {
    let dir = TempDir::new("garbage");
    let out = dir.join("metrics/runs.jsonl");
    let mut child = spawn(&out);
    let mut sent = Vec::new();
    {
        let mut si = child.stdin.take().expect("stdin");
        for (i, e) in events().into_iter().enumerate() {
            if i == 40 {
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
    let last = usage_records(&out).last().cloned().expect("a usage record");
    assert_eq!(last["cacheRead"], TRUE_CACHE_READ);
    assert_eq!(last["messages"], TRUE_MESSAGES);
}
