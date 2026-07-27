{
  description = "issue-pr-cron — pipeline tooling (pr-review-report and future subcommands).";

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
        #
        # `target/` must be excluded explicitly. The crons resolve the flake as
        # `path:$DIR#pr-review-report`, and a `path:` ref copies the working
        # directory as-is — gitignored files included — so cargo's build output
        # sitting inside the crate dir lands in the source and changes its hash.
        # The install dir accumulates ~100MB there, which is why a cron tick
        # could rebuild the crate (and re-run its release-profile test suite)
        # before the model got a single token. `.gitignore` does not save us
        # here: it is not consulted for a `path:` ref.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            # Subtract rather than whitelist src/: a whitelist silently drops
            # anything the crate gains later (tests/, benches/, build.rs), which
            # fails as a missing test gate rather than a loud error.
            (lib.fileset.difference ./pr-review-report-rs (
              lib.fileset.maybeMissing ./pr-review-report-rs/target
            ))
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
      in
      {
        packages.pr-review-report = pr-review-report;
        packages.default = pr-review-report;
      }
    );
}
