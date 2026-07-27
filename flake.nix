{
  description = "issue-pr-cron — pipeline tooling and the cron runners that execute it.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;
        # The crate is a workspace member, so the Cargo.lock lives at the repo
        # root, not in pr-review-report-rs/. buildRustPackage needs the lock
        # inside src, so src is the workspace root — but filtered to just the
        # manifests + crate. Without the filter the whole repo (churning runs/,
        # metrics/, logs) would enter the build and bust the cache every tick.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./pr-review-report-rs
          ];
        };
        # The pipeline's deterministic tooling (queue, report, --commit-closes,
        # --run-metrics, and the migrating recipe subcommands). Tests run in-build
        # via doCheck; invoked directly as `pr-review-report …` on PATH — no wrapper.
        pr-review-report = pkgs.rustPlatform.buildRustPackage {
          pname = "pr-review-report";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;
        };

        # Lift a repo script into a real package: nix builds its PATH from runtimeInputs,
        # and shellcheck runs at build time so a broken runner fails the build rather than
        # the 03:00 cron.
        #
        # `set +o errexit` is restored in each body. writeShellApplication forces
        # `set -euo pipefail`, but these runners are written WITHOUT errexit on purpose —
        # they test `rc` explicitly, use `grep -q … &&` as a conditional, and tolerate
        # best-effort steps. Inheriting errexit would abort a run on the first `grep` that
        # legitimately finds nothing. Each script re-disables it at the top, so the change
        # is visible in the diff rather than implied by the builder.
        runner =
          {
            name,
            file,
            runtimeInputs,
          }:
          pkgs.writeShellApplication {
            inherit name runtimeInputs;
            text = builtins.readFile file;
          };

        # The producer. `gh` and `jq` are here because the MODEL shells out to them from its
        # Bash tool — bare, so campaign-settings.json's deny-list (which matches the literal
        # command text) still applies. `git` likewise: the producer commits and pushes.
        campaign-run = runner {
          name = "campaign-run";
          file = ./campaign-run.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.jq
            pkgs.git
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnugrep
            pkgs.gnused
          ];
        };

        # The vetter. `gh` is for the MCP SERVER, which shells out to it for every GitHub read
        # and its one write; the vetter model itself has no Bash. `jq` is NOT included: the
        # vetter's own jq use was the trace distiller and the metrics merge, both now
        # `pr-review-report` subcommands, and the model cannot invoke anything anyway.
        review-run = runner {
          name = "review-run";
          file = ./review-run.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.git
            pkgs.coreutils
            pkgs.findutils
            pkgs.gnugrep
          ];
        };

        # Data-only refresher on its own cron. Needs git (it commits the snapshot) and
        # util-linux for flock; no jq any more.
        refresh-human-queue = runner {
          name = "refresh-human-queue";
          file = ./refresh-human-queue.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.git
            pkgs.coreutils
            pkgs.util-linux
          ];
        };

        # One-time history backfill; walks git history of the snapshot.
        backfill-human-queue-history = runner {
          name = "backfill-human-queue-history";
          file = ./backfill-human-queue-history.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.git
            pkgs.coreutils
          ];
        };

        # Human entry points. These were bash only because a bare script could not assume
        # `gh` was present and had to build the binary itself; both problems are gone, so
        # what remains is a one-line delegation kept for the existing invocation names.
        review-queue = runner {
          name = "sort-review-queue";
          file = ./sort-review-queue.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.coreutils
          ];
        };
        pr-review-report-sh = runner {
          name = "pr-review-report-sh";
          file = ./pr-review-report.sh;
          runtimeInputs = [
            pr-review-report
            pkgs.gh
            pkgs.coreutils
          ];
        };

        # The exact runtime a human gets when debugging the pipeline by hand: the same
        # pinned gh/jq/pr-review-report the runners execute with.
        #
        # mkShellNoCC, not mkShell: nothing here compiles, and a cc stdenv would put
        # gcc/binutils on PATH for no reason. The Rust toolchain does not enter the closure
        # either — pr-review-report is consumed as a built package.
        #
        # There is deliberately no shellHook: the runners tee stdout into a JSONL trace and
        # anything printed on shell entry corrupts the first line.
        cron-shell = pkgs.mkShellNoCC {
          name = "issue-pr-cron";
          packages = [
            pr-review-report
            pkgs.gh
            pkgs.jq
          ];
        };

        # Building the crate by hand. This exists because the scripts used to run
        # `nix shell nixpkgs#cargo nixpkgs#rustc --command cargo build --release` — an
        # unpinned toolchain, from the registry, producing a binary with no lock
        # relationship to the one `nix build` produces. That is deleted; this shell is
        # where a human iterates instead, on the toolchain the flake already pins.
        rust-shell = pkgs.mkShell {
          name = "issue-pr-cron-rust";
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      in
      {
        packages = {
          inherit
            pr-review-report
            campaign-run
            review-run
            refresh-human-queue
            backfill-human-queue-history
            review-queue
            ;
          pr-review-report-sh = pr-review-report-sh;
          default = pr-review-report;
        };

        # `nix run .#campaign-run` / `.#review-run` — what the crontab invokes.
        apps = {
          campaign-run = {
            type = "app";
            program = "${campaign-run}/bin/campaign-run";
          };
          review-run = {
            type = "app";
            program = "${review-run}/bin/review-run";
          };
          refresh-human-queue = {
            type = "app";
            program = "${refresh-human-queue}/bin/refresh-human-queue";
          };
        };

        devShells.cron = cron-shell;
        devShells.rust = rust-shell;
        # A bare `nix develop` otherwise falls back to the default *package's* build
        # environment, where jq and pr-review-report are absent and gh leaks in from the
        # user's ambient profile — i.e. `nix develop` gave LESS pinning than `nix shell`,
        # which is why the runners avoided it (#76 item 4).
        devShells.default = cron-shell;
      }
    );
}
