//! Behavioural tests for `pr-review-report distill-trace`.
//!
//! Attribution is a property of the STREAM, not of an event. The tag a line wears is minted on an
//! earlier line and has to still be there thousands of events later, so the thing that makes a
//! fan-out log readable is that ONE `TraceDistiller` sees every event. A unit test holding its own
//! distiller cannot show that: rebuild it per event and every unit test still passes while the
//! shipped subcommand renders every agent as `a1`. So these drive the real binary over a real
//! multi-event stream, which is the only place that property is observable.
//!
//! They live in `tests/` for the reasons `require_qa_block.rs` gives: the subject is
//! `env!("CARGO_BIN_EXE_pr-review-report")`, a build artefact of this crate, so the suite runs
//! wherever cargo does — including inside the nix derivation's check phase.

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed a trace to `distill-trace` on stdin and return what it wrote to stdout.
fn distill(trace: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pr-review-report"))
        .arg("distill-trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn distill-trace");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(trace.as_bytes())
        .expect("write trace");
    let out = child.wait_with_output().expect("wait for distill-trace");
    assert!(
        out.status.success(),
        "distill-trace exited {:?}",
        out.status
    );
    String::from_utf8(out.stdout).expect("stdout is utf-8")
}

/// One event per line, the shape the harness writes.
fn line(ev: serde_json::Value) -> String {
    format!("{ev}\n")
}

/// The tag map spans the WHOLE stream, which is what a dispatching run's log needs: run
/// `20260802T130003Z` dispatched its agents up front and their calls then arrived interleaved,
/// hundreds of events after the `Agent` line that names them. A distiller rebuilt per event mints
/// `a1` every time — 19 owners rendered as one, which is the unreadable log this exists to fix.
#[test]
fn one_distiller_holds_the_tags_across_the_whole_stream() {
    let dispatch = |id: &str, desc: &str| {
        line(serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Agent","id":id,"input":{"description":desc}}]}}))
    };
    let call = |parent: Option<&str>, cmd: &str| {
        let mut ev = serde_json::json!({"type":"assistant",
            "message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":cmd}}]}});
        if let Some(p) = parent {
            ev["parent_tool_use_id"] = serde_json::json!(p);
        }
        line(ev)
    };

    let mut trace = String::new();
    trace.push_str(&dispatch("toolu_1", "Rework cyclo.site 400"));
    trace.push_str(&dispatch("toolu_2", "Rework rain.flare 186"));
    // The main loop's own turns keep arriving between a dispatch and its agent's first call.
    for i in 0..50 {
        trace.push_str(&call(None, &format!("gh pr view {i}")));
    }
    trace.push_str(&call(Some("toolu_2"), "git -C /w/flare push"));
    trace.push_str(&call(Some("toolu_1"), "git -C /w/cyclo push"));
    trace.push_str(&call(Some("toolu_2"), "git -C /w/flare status"));
    trace.push_str(&line(
        serde_json::json!({"type":"result","subtype":"success",
        "origin":{"kind":"task-notification"},"result":"rain.flare 186 reworked"}),
    ));
    trace.push_str(&line(
        serde_json::json!({"type":"result","subtype":"success","result":"3 items"}),
    ));

    let out = distill(&trace);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 57, "one line per rendered event: {out}");
    assert_eq!(lines[0], "  ▸ Agent  [a1] Rework cyclo.site 400");
    assert_eq!(lines[1], "  ▸ Agent  [a2] Rework rain.flare 186");
    assert_eq!(lines[2], "  ▸ Bash  gh pr view 0");
    assert_eq!(lines[51], "  ▸ Bash  gh pr view 49");
    // Fifty events after its `Agent` line, a2 is still a2 — and a1 is still a1.
    assert_eq!(lines[52], "  [a2] ▸ Bash  git -C /w/flare push");
    assert_eq!(lines[53], "  [a1] ▸ Bash  git -C /w/cyclo push");
    assert_eq!(lines[54], "  [a2] ▸ Bash  git -C /w/flare status");
    assert_eq!(lines[55], "  [task] ⟹ SUCCESS: rain.flare 186 reworked");
    assert_eq!(lines[56], "  ⟹ SUCCESS: 3 items");
}

/// The run's OWN result is the line a human reads, so it stays distinguishable from the task
/// reports beside it however many of those there are.
#[test]
fn the_runs_own_result_stays_apart_from_the_task_reports() {
    let trace = line(serde_json::json!({"type":"result","subtype":"success",
        "origin":{"kind":"task-notification"},"result":"agent one done"}))
        + &line(serde_json::json!({"type":"result","subtype":"success",
            "origin":{"kind":"task-notification"},"result":"agent two done"}))
        + &line(serde_json::json!({"type":"result","subtype":"success","result":"3 items, 2 PRs"}));
    assert_eq!(
        distill(&trace),
        "  [task] ⟹ SUCCESS: agent one done\n\
         \x20 [task] ⟹ SUCCESS: agent two done\n\
         \x20 ⟹ SUCCESS: 3 items, 2 PRs\n"
    );
}

/// A run that dispatches nothing is untagged THROUGHOUT, byte for byte what a stateless distiller
/// wrote. `parent_tool_use_id` is on more than sub-agent events, which is the trap: run
/// `20260804T114433Z` dispatched nothing and still carried twelve of them, every one a
/// `tool_progress` for a BACKGROUND BASH TASK that renders no line. A number minted there would tag
/// nothing and shift every later agent — so a silent event must not touch the tag map at all.
#[test]
fn a_run_that_dispatches_nothing_renders_untagged_throughout() {
    let trace = String::new()
        + &line(serde_json::json!({"type":"system","subtype":"init"}))
        + &line(serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"loading state"}]}}))
        + &line(serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Bash","input":{"command":"gh pr list"}}]}}))
        + &line(serde_json::json!({"type":"tool_progress","parent_tool_use_id":"toolu_bg"}))
        + "\n"
        + &line(serde_json::json!({"type":"user","parent_tool_use_id":"toolu_bg"}))
        + &line(
            serde_json::json!({"type":"assistant","parent_tool_use_id":"toolu_bg",
            "message":{"content":[{"type":"thinking","thinking":"hidden"}]}}),
        )
        + &line(serde_json::json!({"type":"result","subtype":"success","result":"3 items"}))
        + "{\"type\":\"assis";

    assert_eq!(
        distill(&trace),
        "  · loading state\n  ▸ Bash  gh pr list\n  ⟹ SUCCESS: 3 items\n",
        "no tag anywhere, and a blank or torn line loses nothing but itself"
    );
}
