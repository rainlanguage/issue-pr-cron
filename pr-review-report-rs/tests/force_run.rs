//! Behavioural tests for `pr-review-report force-run`: the observation run as a typed call (#252).
//!
//! NO RUN IS EVER STARTED HERE. A producer or vetter run costs real money and both `DISABLED`
//! flags are in place; what these drive is the half of the subcommand that decides whether a run
//! may start at all — the fast-forward, its refusal, and the argv that would follow — through
//! `--no-run`. The one test that spawns for real does so against a PATH where `nix` does not
//! exist, so the runner still cannot start. The streaming half is a pure function over bytes and
//! is unit-tested beside its own code.
//!
//! The fixtures are REAL git repositories against `file://` remotes, for the reason the crate's
//! flake already states about its clone tests: fast-forwardability is a property of git's own
//! refusal, and a fake would assert our belief about it rather than what git does.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "force-run-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// git with the caller's own config kept out of it — a global `pull.rebase` would otherwise decide
/// what this test measures.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}{}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, file: &str, body: &str) {
    std::fs::write(dir.join(file), body).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", body]);
}

/// A bare "origin" plus an install-dir clone of it, both isolated from the caller's git config.
fn install_dir_with_remote(tag: &str) -> (PathBuf, PathBuf) {
    let root = tmp_dir(tag);
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--bare", "--initial-branch=main", "."]);

    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "--initial-branch=main", "."]);
    commit(&seed, "flake.nix", "seed");
    git(
        &seed,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    git(&seed, &["push", "-u", "origin", "main"]);

    let install = root.join("install");
    git(
        &root,
        &[
            "clone",
            &origin.display().to_string(),
            &install.display().to_string(),
        ],
    );
    (install, seed)
}

fn force_run(args: &[&str], install: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review-report"));
    cmd.arg("force-run")
        .args(args)
        // Never inherited: the whole point of the refusal below is that an install dir is named,
        // not guessed, so the test must not be handed one by the environment it runs in.
        .env_remove("INSTALL_DIR")
        .env_remove("CRON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(dir) = install {
        cmd.arg("--install-dir").arg(dir);
    }
    cmd.output().expect("spawn force-run")
}

/// The install dir lands verbatim inside a `git+file://` URL, where a `..` component resolves
/// against nothing — so what the plan names must be the CANONICAL path, not the string handed in.
/// The same canonical dir rides to the runner as `CRON_DIR`: both runner scripts resolve
/// `DIR="${CRON_DIR:-$PWD}"`, so a child without it inherits the caller's cwd instead — silently
/// starting a PAID run against the wrong dir when that cwd is some checkout of this repo, and
/// refusing at startup anywhere else (#264).
#[test]
fn the_dir_in_the_flake_ref_is_canonical() {
    let (install, _seed) = install_dir_with_remote("canonical");
    let indirect = install.join("..").join("install");
    let out = force_run(&["producer", "--no-run"], Some(&indirect));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let would_run = stdout
        .lines()
        .find(|l| l.contains("git+file://"))
        .unwrap_or_else(|| panic!("no flake ref printed:\n{stdout}{stderr}"));
    assert!(!would_run.contains(".."), "{would_run}");
    let canonical = install.canonicalize().unwrap();
    assert!(
        would_run.contains(&format!("git+file://{}#campaign-run", canonical.display())),
        "{would_run}"
    );
    assert!(
        would_run.contains(&format!("CRON_DIR={} nix run", canonical.display())),
        "{would_run}"
    );
}

/// `INSTALL_DIR` is the environment the runners themselves export, so a human already inside that
/// environment does not have to retype it.
#[test]
fn the_install_dir_may_come_from_the_environment() {
    let (install, _seed) = install_dir_with_remote("env");
    let out = Command::new(env!("CARGO_BIN_EXE_pr-review-report"))
        .args(["force-run", "vetter", "--no-run"])
        .env("INSTALL_DIR", &install)
        .env_remove("CRON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn force-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("#review-run"), "{stdout}{stderr}");
}

/// `CRON_DIR` names the same dir under the convention every runner script resolves, so it works
/// as the fallback too — after `INSTALL_DIR`, whose more specific name wins when both are set.
/// The second invocation proves the precedence: its `CRON_DIR` points at a dir that does not
/// exist, so were it read first, resolution would fail instead of exiting 0.
#[test]
fn cron_dir_is_an_install_dir_fallback_but_install_dir_wins() {
    let (install, _seed) = install_dir_with_remote("cron-dir-env");
    let run = |envs: &[(&str, &std::ffi::OsStr)]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review-report"));
        cmd.args(["force-run", "vetter", "--no-run"])
            .env_remove("INSTALL_DIR")
            .env_remove("CRON_DIR")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn force-run")
    };

    let out = run(&[("CRON_DIR", install.as_os_str())]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("#review-run"), "{stdout}{stderr}");

    let bogus = install.join("not-here");
    let out = run(&[
        ("INSTALL_DIR", install.as_os_str()),
        ("CRON_DIR", bogus.as_os_str()),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("#review-run"), "{stdout}{stderr}");
}

/// The plan exports its own `CRON_DIR` to the child, so an exported `CRON_DIR` that disagrees
/// with the resolved dir loses — and the override is SAID, not silent. An exported `CRON_DIR`
/// that is the same dir spelled differently is not a disagreement.
#[test]
fn an_overridden_ambient_cron_dir_is_said_out_loud() {
    let (install, _seed) = install_dir_with_remote("cron-dir-note");
    let elsewhere = tmp_dir("cron-dir-note-elsewhere");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review-report"));
    cmd.args(["force-run", "vetter", "--no-run"])
        .arg("--install-dir")
        .arg(&install)
        .env_remove("INSTALL_DIR")
        .env("CRON_DIR", &elsewhere)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = cmd.output().expect("spawn force-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("is overridden by CRON_DIR="),
        "{stdout}{stderr}"
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review-report"));
    cmd.args(["force-run", "vetter", "--no-run"])
        .arg("--install-dir")
        .arg(&install)
        .env_remove("INSTALL_DIR")
        .env("CRON_DIR", install.join("..").join("install"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = cmd.output().expect("spawn force-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(!stdout.contains("overridden"), "{stdout}{stderr}");
}

/// A runner that cannot START is the tool's own diagnostic — exit 2, the failure named — never a
/// run result. Nothing ran, so nothing may exit looking like a run that ran and failed, and the
/// `log:` line must not be the last word on a log the runner never wrote.
#[test]
fn a_runner_that_cannot_start_is_a_diagnostic_not_a_run_result() {
    let (install, _seed) = install_dir_with_remote("no-nix");
    // A PATH holding git alone: the fast-forward works, and `nix` cannot resolve — so the spawn
    // fails and no run can start.
    let bin = tmp_dir("no-nix-bin");
    let real_git = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("git"))
        .find(|g| g.is_file())
        .expect("git on PATH");
    std::os::unix::fs::symlink(real_git, bin.join("git")).expect("symlink git");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pr-review-report"));
    cmd.args(["force-run", "vetter"])
        .arg("--install-dir")
        .arg(&install)
        .env("PATH", &bin)
        .env_remove("INSTALL_DIR")
        .env_remove("CRON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = cmd.output().expect("spawn force-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stdout}{stderr}");
    assert!(stderr.contains("cannot start"), "{stdout}{stderr}");
    // It got PAST the fast-forward: the failure is the runner's absence, not the checkout's
    // state.
    assert!(stdout.contains("already current at"), "{stdout}{stderr}");
}

/// An EMPTY `INSTALL_DIR` is not an install dir. Taken literally it canonicalises to the process's
/// own working directory, which for a run forced from a work clone is the wrong repository
/// entirely — so it is refused exactly as an absent one is.
#[test]
fn an_empty_install_dir_in_the_environment_is_not_an_install_dir() {
    let out = Command::new(env!("CARGO_BIN_EXE_pr-review-report"))
        .args(["force-run", "producer", "--no-run"])
        .env("INSTALL_DIR", "")
        .output()
        .expect("spawn force-run");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--install-dir"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The ordinary path: the install dir is behind its remote, so it is fast-forwarded FIRST and the
/// runner that would then build from it is named. `git+file://` builds from the dir's own git
/// HEAD, so a run started before this moved would have exercised the old code — which is what
/// happened twice on 2026-08-09.
#[test]
fn a_stale_install_dir_is_fast_forwarded_before_the_runner_is_named() {
    let (install, seed) = install_dir_with_remote("stale");
    let before = git(&install, &["rev-parse", "--short", "HEAD"]);
    commit(&seed, "campaign-prompt.txt", "newer");
    git(&seed, &["push", "origin", "main"]);

    let out = force_run(&["producer", "--no-run"], Some(&install));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let after = git(&install, &["rev-parse", "--short", "HEAD"]);
    assert_ne!(before, after, "the install dir was not moved:\n{stdout}");
    assert!(stdout.contains(&format!("{before} -> {after}")), "{stdout}");
    // The runner it would build, and where the same bytes land for `watch-run` to reattach to.
    assert!(stdout.contains("#campaign-run"), "{stdout}");
    assert!(stdout.contains("-- --force"), "{stdout}");
    assert!(stdout.contains("campaign.log"), "{stdout}");
    assert!(stdout.contains("would run:"), "{stdout}");
}

/// Already current is not an error, and says so rather than claiming a move.
#[test]
fn an_install_dir_already_current_says_so() {
    let (install, _seed) = install_dir_with_remote("current");
    let out = force_run(&["vetter", "--no-run"], Some(&install));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("already current at"), "{stdout}");
    assert!(!stdout.contains(" -> "), "{stdout}");
    // The role picks the runner, and the vetter's is not the producer's.
    assert!(stdout.contains("#review-run"), "{stdout}");
    assert!(stdout.contains("review.log"), "{stdout}");
}

/// THE STOP THIS SUBCOMMAND OWNS. A diverged install dir cannot be fast-forwarded, and forcing a
/// run against it would exercise neither the code on the branch nor the code on the remote. It is
/// a correctness stop: `--force` walks POLICY stops, and this is not one.
#[test]
fn an_install_dir_that_will_not_fast_forward_is_a_stop() {
    let (install, seed) = install_dir_with_remote("diverged");
    // Both sides commit different content to the same file: no fast-forward exists.
    commit(&install, "campaign-prompt.txt", "local");
    commit(&seed, "campaign-prompt.txt", "remote");
    git(&seed, &["push", "origin", "main"]);
    let head_before = git(&install, &["rev-parse", "HEAD"]);

    let out = force_run(&["producer", "--no-run"], Some(&install));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr:\n{stderr}");
    assert!(stderr.contains("will not fast-forward"), "{stderr}");
    assert!(stderr.contains("STOP"), "{stderr}");
    // It stopped BEFORE naming a runner, and left the checkout exactly as it found it.
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("would run:"),
        "a refused fast-forward still offered a runner"
    );
    assert_eq!(head_before, git(&install, &["rev-parse", "HEAD"]));
}

/// A role outside the vocabulary is refused rather than defaulted. Defaulting would start the
/// producer for a typo'd `vetter`, which is a whole run spent on the wrong queue.
#[test]
fn an_unknown_role_is_refused() {
    let (install, _seed) = install_dir_with_remote("role");
    let out = force_run(&["prod", "--no-run"], Some(&install));
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("producer"), "{stderr}");
    assert!(stderr.contains("vetter"), "{stderr}");
}

/// The install dir is NAMED, never guessed — not from the cwd, not from a default path. A run
/// forced against the wrong install dir spends real money somewhere nobody is watching.
#[test]
fn a_missing_install_dir_is_refused_rather_than_guessed() {
    let out = force_run(&["producer", "--no-run"], None);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--install-dir"), "{stderr}");
    assert!(stderr.contains("INSTALL_DIR"), "{stderr}");
}

/// An install dir that is not there at all fails on the path rather than on git's confusion about
/// it.
#[test]
fn an_install_dir_that_does_not_exist_is_named_in_the_error() {
    let missing = tmp_dir("missing").join("not-here");
    let out = force_run(&["producer", "--no-run"], Some(&missing));
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot resolve install dir"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
