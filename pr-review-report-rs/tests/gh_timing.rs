//! Behavioural tests for `PRR_GH_TIMING`, the per-`gh`-call timing.
//!
//! These drive the REAL binary against a REAL child `gh` — a shim that sleeps a known interval —
//! because the two claims worth holding are both properties of the process:
//!
//! - the timing is the CHILD's wall time, which a unit test over a formatter cannot show;
//! - it never reaches STDOUT. On `mcp` stdout is the JSON-RPC stream, so a timing line there is a
//!   protocol violation, and the only proof is parsing what the process actually wrote.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// The binary cargo just built for this test. Never a PATH lookup.
const BIN: &str = env!("CARGO_BIN_EXE_pr-review-report");

/// What the shim sleeps, and the floor a measurement of it must clear. The floor is under the sleep
/// so a loaded runner cannot make a correct measurement fail.
const SHIM_SLEEP_SECS: &str = "0.4";
const FLOOR_MS: u64 = 200;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("prr-gh-timing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A directory holding a `gh` that sleeps and then answers with an empty JSON document, to be put
/// FIRST on PATH. Every `gh` call the binary makes then costs a known interval and reaches no
/// network.
fn gh_shim(tag: &str) -> (TempDir, std::ffi::OsString) {
    let dir = TempDir::new(tag);
    let gh = dir.0.join("gh");
    std::fs::write(
        &gh,
        format!("#!/bin/sh\nsleep {SHIM_SLEEP_SECS}\necho '{{}}'\n"),
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut dirs = vec![dir.0.clone()];
            dirs.extend(std::env::split_paths(&p));
            std::env::join_paths(dirs).expect("join PATH")
        }
        None => dir.0.clone().into_os_string(),
    };
    (dir, path)
}

/// The milliseconds a `gh-timing:` per-call line reports.
fn call_ms(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("gh-timing: ")?;
    rest.split_once("ms ")?.0.parse().ok()
}

fn trusted_comments(timing: Option<&str>) -> (String, String) {
    let (_dir, path) = gh_shim(timing.unwrap_or("unset"));
    let mut cmd = Command::new(BIN);
    cmd.args(["trusted-comments", "rainlanguage/rainix", "42"])
        .env("PATH", &path);
    match timing {
        Some(v) => cmd.env("PRR_GH_TIMING", v),
        None => cmd.env_remove("PRR_GH_TIMING"),
    };
    let out = cmd.output().expect("run pr-review-report");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Unset, the run is byte-identical to what it always was. This is what makes the instrumentation
/// safe to leave in the shipped binary. `0` and empty are off too, so a wrapper that always exports
/// the variable can still turn it off with a value.
#[test]
fn without_the_env_var_nothing_is_emitted() {
    for off in [None, Some("0"), Some("")] {
        let (stdout, stderr) = trusted_comments(off);
        assert!(!stderr.contains("gh-timing:"), "{off:?} stderr: {stderr}");
        assert!(!stdout.contains("gh-timing:"), "{off:?} stdout: {stdout}");
    }
}

/// Set, the per-call line reports the CHILD's wall time and names the call, the summary closes the
/// invocation, and stdout carries neither.
#[test]
fn the_env_var_times_the_child_on_stderr_only() {
    let (stdout, stderr) = trusted_comments(Some("1"));
    assert!(!stdout.contains("gh-timing"), "stdout: {stdout}");

    let call: Vec<&str> = stderr
        .lines()
        .filter(|l| {
            l.starts_with("gh-timing: ") && !l.contains(" calls, ") && !l.contains("slowest")
        })
        .collect();
    assert_eq!(call.len(), 1, "one call was made: {stderr}");
    assert!(
        call[0].contains("pr view 42 rainlanguage/rainix"),
        "the line attributes the call: {}",
        call[0]
    );
    let ms = call_ms(call[0]).unwrap_or_else(|| panic!("no ms in {}", call[0]));
    assert!(
        ms >= FLOOR_MS,
        "the shim slept {SHIM_SLEEP_SECS}s; {ms}ms is not the child's wall time"
    );

    assert!(
        stderr.contains("gh-timing: trusted-comments: 1 calls,"),
        "the invocation is summarised under its subcommand: {stderr}"
    );
    assert!(
        stderr.contains("gh-timing: trusted-comments: slowest"),
        "the summary names the slowest call: {stderr}"
    );
}

/// THE constraint: on the MCP server stdout is the protocol. Every line it writes must still parse
/// as JSON with the timing on, and the summary must be attributed to the TOOL CALL — the unit a
/// client waits on — not to the server process, which outlives every call it serves.
#[test]
fn the_mcp_server_keeps_its_protocol_stream_clean() {
    let (_dir, path) = gh_shim("mcp");
    let mut child = Command::new(BIN)
        .args(["mcp", "--profile", "vetter"])
        .env("PATH", &path)
        .env("PRR_GH_TIMING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp server");

    let mut stdin = child.stdin.take().expect("stdin");
    for req in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"unvetted","arguments":{}}}"#,
    ] {
        writeln!(stdin, "{req}").expect("write request");
    }
    stdin.flush().expect("flush");
    drop(stdin);

    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let mut replies = 0;
    for line in BufReader::new(stdout.as_bytes()).lines() {
        let line = line.expect("stdout line");
        serde_json::from_str::<serde_json::Value>(&line)
            .unwrap_or_else(|e| panic!("stdout is not JSON-RPC ({e}): {line}"));
        replies += 1;
    }
    assert_eq!(replies, 2, "one reply per request: {stdout}");

    assert!(
        stderr.contains("gh-timing: unvetted: "),
        "the tool call is its own span: {stderr}"
    );
    assert!(
        !stderr.contains("gh-timing: mcp: "),
        "a call already reported under its tool is not re-reported at exit: {stderr}"
    );
}
