//! Behavioural tests for `pr-review-report watch-run`: the live follow a human watches a FORCED
//! run through (#252).
//!
//! The filter itself is a pure function and is unit-tested beside its own code. What cannot be
//! observed there is the property that cost an afternoon: WHERE the follow starts. `tail -f`
//! without `-n 0` replays the log's existing tail, so the previous run's `SKIP`/`END` lines arrive
//! as events for the current one — twice on 2026-08-09 that produced a false report to the human,
//! once "the run skipped" and once "the run ended". A unit test over the filter passes either way;
//! only driving the real binary against a file that already had content shows it.
//!
//! No forced run is involved. Both `DISABLED` flags are in place, a real producer run costs real
//! money, and the thing under test is a reader of bytes — so the run is a synthetic writer
//! appending the exact lines the runners write.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A scratch dir under the target dir, so the suite writes only where cargo already does.
fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "watch-run-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn append(path: &std::path::Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("append to log");
    writeln!(f, "{line}").expect("write line");
    f.flush().expect("flush");
}

/// Spawn the watcher, give it a moment to record the log's length, then run `writer`, then read
/// what the watcher printed and how it exited.
fn watch(log: &std::path::Path, timeout_secs: u64, writer: impl FnOnce()) -> (String, i32) {
    let child = Command::new(env!("CARGO_BIN_EXE_pr-review-report"))
        .arg("watch-run")
        .arg(log)
        .arg("--timeout-secs")
        .arg(timeout_secs.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn watch-run");
    // The starting offset is taken when the process starts, so the writer must not race it.
    std::thread::sleep(Duration::from_millis(700));
    writer();
    let out = child.wait_with_output().expect("wait for watch-run");
    (
        String::from_utf8(out.stdout).expect("stdout is utf-8"),
        out.status.code().unwrap_or(-1),
    )
}

fn append_partial(path: &std::path::Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("append to log");
    write!(f, "{text}").expect("write partial line");
    f.flush().expect("flush");
}

/// The watcher polls a file another process is WRITING, so it routinely reads a line the runner has
/// not finished. A line with no terminator must be left where it is and re-read whole on the next
/// pass: consuming its offset splits one log line across two reads, and each half then fails the
/// filter on its own — the marker lands in neither, and the line the human is waiting for never
/// appears.
#[test]
fn a_half_written_line_is_shown_once_and_whole() {
    let dir = tmp_dir("torn");
    let log = dir.join("campaign.log");
    let (out, code) = watch(&log, 30, || {
        append_partial(&log, "  · a narration line the runner has not finished");
        // Longer than the watcher's poll interval, so it definitely reads the torn tail.
        std::thread::sleep(Duration::from_millis(600));
        append(&log, " writing yet");
        append(&log, "2026-08-09T16:06:26Z campaign run END (exit=0)");
    });
    assert_eq!(code, 0);
    assert!(
        out.contains("a narration line the runner has not finished writing yet"),
        "the line did not arrive whole:\n{out}"
    );
    assert_eq!(
        out.matches("a narration line").count(),
        1,
        "the line was emitted more than once:\n{out}"
    );
}

/// The bug that produced two false reports. The log already holds a previous run's whole
/// lifecycle; the watcher must attribute NONE of it to the run starting now, and must not stop on
/// the `END` line that is already there.
#[test]
fn the_previous_runs_tail_is_never_replayed_as_this_runs_events() {
    let dir = tmp_dir("no-replay");
    let log = dir.join("campaign.log");
    for line in [
        "2026-08-09T14:51:50Z campaign run START (model=opus, host=box)",
        "  · the previous run's narration",
        "2026-08-09T15:00:00Z campaign run END (exit=0, trace=runs/old.jsonl)",
        "2026-08-09T15:47:35Z SKIP: previous run still holding the lock",
    ] {
        append(&log, line);
    }
    let (out, code) = watch(&log, 30, || {
        append(
            &log,
            "2026-08-09T15:52:41Z campaign run START (model=opus, host=box)",
        );
        append(&log, "  · loading the pipeline state");
        append(
            &log,
            "2026-08-09T16:06:26Z campaign run END (exit=0, trace=runs/new.jsonl)",
        );
    });
    assert_eq!(code, 0, "the watch should end on THIS run's END line");
    assert!(
        !out.contains("the previous run's narration"),
        "replayed the old tail:\n{out}"
    );
    assert!(
        !out.contains("runs/old.jsonl"),
        "replayed the old END:\n{out}"
    );
    assert!(
        !out.contains("still holding the lock"),
        "replayed the old SKIP:\n{out}"
    );
    assert!(out.contains("loading the pipeline state"), "{out}");
    assert!(out.contains("runs/new.jsonl"), "{out}");
}

/// The other hand-written mistake: a filter that carried only the lifecycle lines, so a narrating
/// run looked silent. Driven end to end because what a human sees is the binary's stdout, and the
/// noise it must NOT carry is in the same stream.
#[test]
fn the_stream_carries_narration_tool_calls_and_results_and_drops_the_rest() {
    let dir = tmp_dir("signal");
    let log = dir.join("campaign.log");
    let (out, code) = watch(&log, 30, || {
        for line in [
            "2026-08-09T15:52:41Z campaign run START (model=opus, host=box)",
            "  ok=gh,jq,forge,cargo",
            "  · narrating what I am about to do",
            "  ▸ Bash  pr-review-report state-load --json",
            "  ⟹ SUCCESS: opened rainlanguage/rain.math#12",
            "  !! model opus is quota-limited — falling back to next model",
            "  {\"outage\":false}",
            "2026-08-09T16:06:26Z campaign run END (exit=0, trace=runs/new.jsonl)",
        ] {
            append(&log, line);
        }
    });
    assert_eq!(code, 0);
    for shown in [
        "campaign run START",
        "narrating what I am about to do",
        "▸ Bash",
        "⟹ SUCCESS",
        "quota-limited",
        "campaign run END",
    ] {
        assert!(out.contains(shown), "dropped {shown:?} from:\n{out}");
    }
    for hidden in ["ok=gh,jq", "outage"] {
        assert!(!out.contains(hidden), "passed {hidden:?} in:\n{out}");
    }
}

/// A log that does not exist yet is not an error. A forced run is started in one call and watched
/// in the next, and on a fresh install the log's first byte is written by the run being watched.
#[test]
fn a_log_that_does_not_exist_yet_is_waited_for() {
    let dir = tmp_dir("absent");
    let log = dir.join("campaign.log");
    assert!(!log.exists());
    let (out, code) = watch(&log, 30, || {
        append(
            &log,
            "2026-08-09T15:52:41Z campaign run START (model=opus, host=box)",
        );
        append(&log, "2026-08-09T16:06:26Z campaign run END (exit=0)");
    });
    assert_eq!(code, 0);
    assert!(out.contains("campaign run START"), "{out}");
}

/// The deadline stops the WATCH, never the run — so it is a distinct exit code (3, as `await`
/// uses) rather than a success, and everything seen before it still reached the human.
#[test]
fn the_deadline_exits_three_with_what_it_saw() {
    let dir = tmp_dir("deadline");
    let log = dir.join("campaign.log");
    let (out, code) = watch(&log, 2, || {
        append(&log, "  · the run is still going");
    });
    assert_eq!(code, 3, "a deadline is not a completed run");
    assert!(out.contains("the run is still going"), "{out}");
}

/// A `SKIP`/`ABORT` ends the watch as surely as an `END` does — those runs never write an `END`
/// line at all, and a watcher that waited for one would hold the caller's turn open for the whole
/// timeout on the fastest possible outcome.
#[test]
fn a_skipped_run_ends_the_watch_too() {
    let dir = tmp_dir("skip");
    let log = dir.join("campaign.log");
    let (out, code) = watch(&log, 30, || {
        append(&log, "2026-08-09T15:52:41Z usage-gate: PAUSE 91% used");
        append(&log, "2026-08-09T15:52:41Z SKIP: DISABLED flag present");
    });
    assert_eq!(code, 0);
    assert!(out.contains("usage-gate: PAUSE"), "{out}");
    assert!(out.contains("SKIP: DISABLED"), "{out}");
}
