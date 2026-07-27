#!/usr/bin/env bash
# pr-review-report.sh — report every open PR (and logged close-candidate) that needs a HUMAN
# decision, reading verdict state from GitHub labels and its own review
# state on top of the CI/mergeability signal. Delegates to the Rust implementation in
# pr-review-report-rs/. Everything prints as full clickable URLs.
#
# Usage:   ./pr-review-report.sh            # all buckets
#          ./pr-review-report.sh --ready    # only the reviewed-&-ready-to-merge bucket
# Config from ./cron.env (ORG, PR_ASSIGNEE), read by the binary.
set -uo pipefail

# Packaged as a flake output, so `gh` and the binary are already on PATH from the flake's locked
# nixpkgs. Everything this wrapper used to do — re-exec under a registry-resolved `nix shell` to
# find gh, then `cargo build --release` under an unpinned registry toolchain if the binary was
# missing — is gone. Both existed only because a bare script could assume neither its tools nor
# its own build; a flake package has both by construction, and the ad-hoc cargo build produced a
# binary with no lock relationship to the one `nix build` produces (#76 items 2 and 5).
#
# $0 is a read-only nix store path now, so the install dir (cron.env, ledgers) comes from
# $CRON_DIR, defaulting to $PWD — which is the checkout for an interactive run.
cd "${CRON_DIR:-$PWD}" || exit 1

exec pr-review-report "$@"
