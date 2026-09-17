#!/usr/bin/env bash
# bootstrap.sh — stand the producer/vetter pipeline up on a fresh box.
#
# The install path is TRACKED, not typed: behaviour that lives only on one box
# is behaviour that does not survive moving the FSM to another machine (#282).
# Everything except the two KEY-CUSTODY steps (a `claude` OAuth login and a
# `gh auth login`) is automated here, and every step is idempotent — re-running
# changes nothing that is already right.
#
# The pipeline is installed PAUSED: `DISABLED` and `review-DISABLED` are written
# BEFORE the crontab is, so standing a box up is never the same act as starting
# it. Two producers over one ORGS open DUPLICATE PRs for the same issues, so
# resuming is a human `rm` taken only after the old box is confirmed stopped.
# See README.md, "Standing up a fresh box — bootstrap.sh".
#
# Rehearse with --dry-run: it prints every action and mutates nothing.

set -euo pipefail

REPO_URL_DEFAULT="https://github.com/rainlanguage/issue-pr-cron.git"
NIX_INSTALLER_URL="https://nixos.org/nix/install"
CLAUDE_INSTALLER_URL="https://claude.ai/install.sh"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# Every key this script can write into `cron.env`. `cron.env.example` is the
# CONTRACT: a key here that the example does not carry aborts the run, because a
# knob the example never documented is a knob nothing else in the repo honours.
# CRON_FORCE is deliberately absent and asserted absent — a variable would force
# every scheduled tick, silently and for ever, and both runners exit 2 while it
# is set (#245).
INSTALL_DIR="$HOME/issue-pr-cron"
REPO_URL="$REPO_URL_DEFAULT"
DRY_RUN=0
ASSIGNEE=""
GIT_USER_NAME=""
GIT_USER_EMAIL=""

# The runner's own default for WORK_DIR. Needed even when --work-dir is not
# passed, because the nightly `gc` line must name a real clone root.
WORK_DIR="$HOME/code"

declare -A ENV_VALUES=()
# USAGE_HEADROOM_PCT=0 is the DEDICATED-box value, not a new default: the pace
# gate exists so interactive/BAU work keeps standing headroom and the deferrable
# consumer waits (#158), and a box with its own subscription has no interactive
# consumer to leave headroom for. The ceiling is then the only check. It is
# written to `cron.env` only — `cron.env.example` keeps 5, for a shared box.
ENV_VALUES[USAGE_HEADROOM_PCT]="0"

die() {
  printf 'bootstrap: %s\n' "$*" >&2
  exit 1
}

say() { printf '%s\n' "$*"; }

step() { printf '\n== %s\n' "$*"; }

# Every mutation goes through one of these two, so --dry-run is a property of
# the script rather than a flag each step remembers to check.
would() { printf '  [dry-run] would %s\n' "$*"; }

did() { printf '  %s\n' "$*"; }

usage() {
  cat <<'USAGE'
bootstrap.sh — install the issue-pr-cron producer/vetter pipeline on a fresh box.

Usage: ./bootstrap.sh --assignee <github-handle> [options]

Required:
  --assignee HANDLE        PR_ASSIGNEE: the handle every opened PR is assigned
                           to. Prompted for on a TTY if omitted.

Install:
  --install-dir DIR        where the pipeline lives (default: $HOME/issue-pr-cron)
  --repo-url URL           clone source (default: the rainlanguage remote)
  --work-dir DIR           WORK_DIR: where work clones are made (default: $HOME/code)
  --git-user-name NAME     git user.name, if the box has none
  --git-user-email EMAIL   git user.email, if the box has none

cron.env values (anything omitted stays COMMENTED, so the runner's own default
applies; every key must exist in cron.env.example or the run aborts):
  --orgs "A B C"           ORGS — the single source of truth for scope
  --model NAME             MODEL
  --review-model NAME      REVIEW_MODEL
  --fallback-models "A B"  FALLBACK_MODELS
  --maxtime DURATION       MAXTIME
  --keep-runs N            KEEP_RUNS
  --review-maxtime DUR     REVIEW_MAXTIME
  --review-keep-runs N     REVIEW_KEEP_RUNS
  --headroom-pct N         USAGE_HEADROOM_PCT (default 0 — a dedicated box has no
                           interactive consumer to hold headroom for)
  --ceiling-pct N          USAGE_CEILING_PCT

Other:
  --dry-run                print every action, mutate nothing
  --help                   this text

The pipeline is installed PAUSED. Resuming is a human `rm DISABLED review-DISABLED`
AFTER the old box is confirmed stopped — two producers over one ORGS open
duplicate PRs.
USAGE
}

need_value() {
  [ "$2" -gt 1 ] || die "$1 needs a value"
}

while [ "$#" -gt 0 ]; do
  arg="$1"
  val=""
  case "$arg" in
    --*=*)
      val="${arg#*=}"
      arg="${arg%%=*}"
      set -- "$arg" "$val" "${@:2}"
      ;;
  esac
  case "$1" in
    --install-dir) need_value "$1" "$#"; INSTALL_DIR="$2"; shift 2 ;;
    --repo-url) need_value "$1" "$#"; REPO_URL="$2"; shift 2 ;;
    --work-dir)
      need_value "$1" "$#"
      WORK_DIR="$2"
      ENV_VALUES[WORK_DIR]="$2"
      shift 2
      ;;
    --assignee) need_value "$1" "$#"; ASSIGNEE="$2"; shift 2 ;;
    --orgs) need_value "$1" "$#"; ENV_VALUES[ORGS]="$2"; shift 2 ;;
    --model) need_value "$1" "$#"; ENV_VALUES[MODEL]="$2"; shift 2 ;;
    --review-model) need_value "$1" "$#"; ENV_VALUES[REVIEW_MODEL]="$2"; shift 2 ;;
    --fallback-models) need_value "$1" "$#"; ENV_VALUES[FALLBACK_MODELS]="$2"; shift 2 ;;
    --maxtime) need_value "$1" "$#"; ENV_VALUES[MAXTIME]="$2"; shift 2 ;;
    --keep-runs) need_value "$1" "$#"; ENV_VALUES[KEEP_RUNS]="$2"; shift 2 ;;
    --review-maxtime) need_value "$1" "$#"; ENV_VALUES[REVIEW_MAXTIME]="$2"; shift 2 ;;
    --review-keep-runs) need_value "$1" "$#"; ENV_VALUES[REVIEW_KEEP_RUNS]="$2"; shift 2 ;;
    --headroom-pct) need_value "$1" "$#"; ENV_VALUES[USAGE_HEADROOM_PCT]="$2"; shift 2 ;;
    --ceiling-pct) need_value "$1" "$#"; ENV_VALUES[USAGE_CEILING_PCT]="$2"; shift 2 ;;
    --git-user-name) need_value "$1" "$#"; GIT_USER_NAME="$2"; shift 2 ;;
    --git-user-email) need_value "$1" "$#"; GIT_USER_EMAIL="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

[ "$(id -u)" -ne 0 ] || die "do not run this as root. The pipeline installs into a user's \$HOME, uses a single-user nix, and runs from that user's crontab."

if [ -z "$ASSIGNEE" ]; then
  if [ -t 0 ]; then
    read -r -p "PR_ASSIGNEE (the GitHub handle every opened PR is assigned to): " ASSIGNEE
  fi
  [ -n "$ASSIGNEE" ] || die "--assignee is required (PR_ASSIGNEE has no usable default)"
fi
ENV_VALUES[PR_ASSIGNEE]="$ASSIGNEE"

case "$INSTALL_DIR" in
  /*) ;;
  *) die "--install-dir must be an absolute path (the crontab and the flake ref both carry it verbatim)" ;;
esac
case "$WORK_DIR" in
  /*) ;;
  *) die "--work-dir must be an absolute path (the nightly gc line carries it verbatim)" ;;
esac

if [ "$DRY_RUN" -eq 1 ]; then
  say "DRY RUN — nothing on this box will be changed."
fi

# ---------------------------------------------------------------- preflight --
step "preflight"
missing=()
for t in curl git python3 crontab; do
  command -v "$t" >/dev/null 2>&1 || missing+=("$t")
done
if [ "${#missing[@]}" -gt 0 ]; then
  die "missing required tool(s): ${missing[*]}
  curl     — fetches the nix and claude installers
  git      — clones the install dir, and the metrics ledger reaches main by commit
  python3  — BOTH PreToolUse guards parse their hook payload with it and ALLOW
             when it yields nothing, so without python3 the guards are SILENTLY
             INERT rather than absent. It is also how this script edits
             ~/.claude/settings.json.
  crontab  — the schedule"
fi
did "curl, git, python3, crontab present"

# ---------------------------------------------------------------------- nix --
step "nix"
if command -v nix >/dev/null 2>&1; then
  did "nix already installed: $(command -v nix)"
elif [ "$DRY_RUN" -eq 1 ]; then
  would "download $NIX_INSTALLER_URL and run it with --no-daemon (single-user)"
else
  installer="$(mktemp)"
  curl -fsSL "$NIX_INSTALLER_URL" -o "$installer" || die "could not download the nix installer from $NIX_INSTALLER_URL"
  sh "$installer" --no-daemon || die "the nix installer failed"
  rm -f "$installer"
  for profile_script in \
    "$HOME/.nix-profile/etc/profile.d/nix.sh" \
    "$HOME/.local/state/nix/profiles/profile/etc/profile.d/nix.sh" \
    /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh; do
    if [ -e "$profile_script" ]; then
      # shellcheck source=/dev/null
      . "$profile_script"
      break
    fi
  done
  command -v nix >/dev/null 2>&1 || die "nix installed but is not on PATH — open a new login shell and re-run"
  did "installed nix: $(command -v nix)"
fi

# nix.conf. Without experimental-features NOTHING here runs: every cron line and
# every verify below is a flake command. The substituters are what keep a fresh
# box from building the world.
NIX_CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/nix"
NIX_CONF="$NIX_CONF_DIR/nix.conf"
conf_has_key() {
  [ -f "$NIX_CONF" ] && grep -Eq "^[[:space:]]*$1[[:space:]]*=" "$NIX_CONF"
}
conf_append() {
  if [ "$DRY_RUN" -eq 1 ]; then
    would "append to $NIX_CONF: $1"
  else
    mkdir -p "$NIX_CONF_DIR"
    printf '%s\n' "$1" >>"$NIX_CONF"
    did "appended to $NIX_CONF: $1"
  fi
}
if conf_has_key experimental-features; then
  if grep -E "^[[:space:]]*experimental-features[[:space:]]*=" "$NIX_CONF" | grep -q 'nix-command' &&
    grep -E "^[[:space:]]*experimental-features[[:space:]]*=" "$NIX_CONF" | grep -q 'flakes'; then
    did "experimental-features already enables nix-command and flakes"
  else
    die "$NIX_CONF sets experimental-features without nix-command + flakes. Every cron line is a flake command, so fix that line by hand rather than have this script guess at merging it:
  experimental-features = nix-command flakes"
  fi
else
  conf_append "experimental-features = nix-command flakes"
fi
if conf_has_key substituters; then
  did "substituters already configured"
else
  conf_append "substituters = https://cache.nixos.org https://rainlanguage.cachix.org"
fi
if conf_has_key trusted-public-keys; then
  did "trusted-public-keys already configured"
else
  conf_append "trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= rainlanguage.cachix.org-1:2vyHjEDMKtwXwLZ7XPFvOOa9EGpKlNPvIS2FKtwlIVE="
fi

# ------------------------------------------------------------------- claude --
step "claude CLI"
if command -v claude >/dev/null 2>&1; then
  did "claude already installed: $(command -v claude)"
elif [ "$DRY_RUN" -eq 1 ]; then
  would "install the claude CLI from $CLAUDE_INSTALLER_URL into \$HOME/.local/bin"
else
  installer="$(mktemp)"
  curl -fsSL "$CLAUDE_INSTALLER_URL" -o "$installer" || die "could not download the claude installer from $CLAUDE_INSTALLER_URL"
  bash "$installer" || die "the claude installer failed"
  rm -f "$installer"
  export PATH="$PATH:$HOME/.local/bin"
  command -v claude >/dev/null 2>&1 || die "claude installed but is not on PATH — expected \$HOME/.local/bin"
  did "installed claude: $(command -v claude)"
fi

# -------------------------------------------------------------- install dir --
step "install dir: $INSTALL_DIR"
is_this_repo() {
  [ -f "$1/cron.env.example" ] && [ -f "$1/flake.nix" ] && [ -d "$1/.git" ]
}
if [ -d "$INSTALL_DIR" ] && [ -n "$(ls -A "$INSTALL_DIR" 2>/dev/null || true)" ]; then
  is_this_repo "$INSTALL_DIR" ||
    die "$INSTALL_DIR is non-empty and is not an issue-pr-cron checkout. Refusing to clone over it — pick another --install-dir."
  did "reusing the existing checkout at $INSTALL_DIR"
elif [ "$DRY_RUN" -eq 1 ]; then
  would "git clone $REPO_URL $INSTALL_DIR"
else
  git clone "$REPO_URL" "$INSTALL_DIR" || die "clone failed"
  did "cloned $REPO_URL"
fi

# `refresh-human-queue` publishes human-queue.json, human-queue-history.jsonl AND
# metrics/runs.jsonl by committing to main and pushing — it HARD-ERRORS (exit 1)
# when the branch has no upstream, and the run-metrics ledger then silently stops
# publishing while the runners keep appending locally. A fresh clone is on a
# tracking branch; a hand-made directory is not, so this is checked, not assumed.
if [ -d "$INSTALL_DIR/.git" ]; then
  if upstream="$(git -C "$INSTALL_DIR" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)"; then
    did "install dir tracks $upstream — the hourly refresher can publish the metrics ledger"
  else
    die "the checkout at $INSTALL_DIR has no upstream branch. refresh-human-queue exits 1 without one, so human-queue.json and metrics/runs.jsonl would never reach main."
  fi
elif [ "$DRY_RUN" -eq 1 ]; then
  would "assert the fresh clone's branch has an upstream (refresh-human-queue exits 1 without one)"
fi

# ---------------------------------------------------------------------- git --
step "git identity and credentials"
git_global() { git config --global "$@"; }
set_git_global() {
  local key="$1" want="$2" have=""
  have="$(git_global --get "$key" 2>/dev/null || true)"
  if [ "$have" = "$want" ]; then
    did "$key already set"
  elif [ -n "$have" ]; then
    die "git config --global $key is '$have', not '$want'. This box's own gitconfig is the human's; set it deliberately:
  git config --global $key '$want'"
  elif [ "$DRY_RUN" -eq 1 ]; then
    would "git config --global $key '$want'"
  else
    git_global "$key" "$want"
    did "set $key"
  fi
}
# `gh auth git-credential` UNQUALIFIED. A path-qualified helper is a hazard: it
# pins a copy of gh that survives only while that exact path does, and every push
# fails the moment it moves. `gh auth setup-git` is not used either — it writes
# per-host helper lines this script would then have to reconcile on every re-run.
set_git_global credential.helper '!gh auth git-credential'
for pair in "user.name:$GIT_USER_NAME" "user.email:$GIT_USER_EMAIL"; do
  key="${pair%%:*}"
  want="${pair#*:}"
  have="$(git_global --get "$key" 2>/dev/null || true)"
  if [ -n "$want" ]; then
    set_git_global "$key" "$want"
  elif [ -n "$have" ]; then
    did "$key already set to '$have'"
  else
    die "git has no $key and none was passed. The hourly refresher COMMITS the queue snapshot and the metrics ledger, and that commit fails without an identity. Pass --git-user-name / --git-user-email."
  fi
done

# ----------------------------------------------------------------- cron.env --
step "cron.env"
CRON_ENV="$INSTALL_DIR/cron.env"
EXAMPLE="$INSTALL_DIR/cron.env.example"
if [ ! -f "$EXAMPLE" ] && [ "$DRY_RUN" -eq 1 ] && [ -f "$SCRIPT_DIR/cron.env.example" ]; then
  # Rehearsing before the clone exists: the contract check still runs, against
  # this script's own checkout of the example.
  EXAMPLE="$SCRIPT_DIR/cron.env.example"
fi

generate_cron_env() {
  local example="$1"
  local -A seen=()
  local line key emitted re
  while IFS= read -r line || [ -n "$line" ]; do
    emitted=0
    for key in "${!ENV_VALUES[@]}"; do
      re="^#?[[:space:]]*${key}="
      if [[ $line =~ $re ]]; then
        printf '%s="%s"\n' "$key" "${ENV_VALUES[$key]}"
        seen[$key]=1
        emitted=1
        break
      fi
    done
    [ "$emitted" -eq 1 ] || printf '%s\n' "$line"
  done <"$example"
  for key in "${!ENV_VALUES[@]}"; do
    [ -n "${seen[$key]:-}" ] ||
      die "cron.env.example carries no '$key' line. The example is the CONTRACT — a key it does not document is a knob nothing in the repo reads. Add it there first."
  done
}

if [ -f "$CRON_ENV" ]; then
  did "$CRON_ENV already exists — left untouched (it holds this box's own values)"
elif [ ! -f "$EXAMPLE" ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    would "generate $CRON_ENV from cron.env.example (not readable yet — clone first to rehearse the contract check)"
  else
    die "$EXAMPLE is missing from the checkout"
  fi
else
  generated="$(generate_cron_env "$EXAMPLE")"
  # CRON_FORCE is not a setting and must never appear here: it would force EVERY
  # scheduled tick, silently and for ever, and both runners exit 2 while it is
  # set (#245). Asserted on the OUTPUT, so no future edit above can reintroduce it.
  if printf '%s\n' "$generated" | grep -Eq '^[[:space:]]*CRON_FORCE='; then
    die "generated cron.env carries a CRON_FORCE line. Forcing is an argument to ONE invocation (--force), never a variable."
  fi
  if printf '%s\n' "$generated" | grep -Eq '^[[:space:]]*USAGE_SLACK_PCT='; then
    die "generated cron.env carries USAGE_SLACK_PCT, which is RETIRED (#158) — the gate refuses to gate while it is set."
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    would "write $CRON_ENV as:"
    printf '%s\n' "$generated" | grep -Ev '^[[:space:]]*(#|$)' | sed 's/^/      /'
  else
    printf '%s\n' "$generated" >"$CRON_ENV"
    chmod 600 "$CRON_ENV"
    did "wrote $CRON_ENV ($(printf '%s\n' "$generated" | grep -Ecv '^[[:space:]]*(#|$)') settings)"
  fi
fi

# -------------------------------------------------------------------- hooks --
step "PreToolUse hooks in ~/.claude/settings.json"
SETTINGS="$HOME/.claude/settings.json"
for h in block-nix-wrap-gh.sh block-cron-git-bypass.sh; do
  if [ -e "$INSTALL_DIR/hooks/$h" ] && [ ! -x "$INSTALL_DIR/hooks/$h" ]; then
    die "$INSTALL_DIR/hooks/$h is not executable — Claude Code would fail the tool call instead of guarding it"
  fi
done
if [ "$DRY_RUN" -eq 1 ] && [ ! -d "$INSTALL_DIR" ]; then
  would "wire $INSTALL_DIR/hooks/block-nix-wrap-gh.sh and .../block-cron-git-bypass.sh into $SETTINGS as PreToolUse Bash hooks"
else
  DRY_RUN="$DRY_RUN" SETTINGS="$SETTINGS" INSTALL_DIR="$INSTALL_DIR" python3 - <<'PY'
import json, os, shutil, sys, time

settings = os.environ["SETTINGS"]
install_dir = os.environ["INSTALL_DIR"]
dry = os.environ["DRY_RUN"] == "1"
wanted = [
    f"{install_dir}/hooks/block-nix-wrap-gh.sh",
    f"{install_dir}/hooks/block-cron-git-bypass.sh",
]

data = {}
if os.path.exists(settings):
    with open(settings) as fh:
        text = fh.read().strip()
    if text:
        try:
            data = json.loads(text)
        except json.JSONDecodeError as exc:
            sys.exit(f"bootstrap: {settings} is not valid JSON ({exc}); fix it by hand first")
if not isinstance(data, dict):
    sys.exit(f"bootstrap: {settings} does not hold a JSON object")

hooks = data.setdefault("hooks", {})
pre = hooks.setdefault("PreToolUse", [])
if not isinstance(pre, list):
    sys.exit(f"bootstrap: {settings} hooks.PreToolUse is not a list")

matcher = next(
    (e for e in pre if isinstance(e, dict) and e.get("matcher") == "Bash"), None
)
if matcher is None:
    matcher = {"matcher": "Bash", "hooks": []}
    pre.append(matcher)
entries = matcher.setdefault("hooks", [])

added = []
for cmd in wanted:
    if any(isinstance(e, dict) and e.get("command") == cmd for e in entries):
        print(f"  already wired: {cmd}")
        continue
    entries.append({"type": "command", "command": cmd})
    added.append(cmd)

if not added:
    print("  no change")
    raise SystemExit(0)
if dry:
    for cmd in added:
        print(f"  [dry-run] would add PreToolUse Bash hook: {cmd}")
    raise SystemExit(0)

os.makedirs(os.path.dirname(settings), exist_ok=True)
if os.path.exists(settings):
    backup = f"{settings}.bak.{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}"
    shutil.copy2(settings, backup)
    print(f"  backed up {settings} -> {backup}")
tmp = f"{settings}.tmp.{os.getpid()}"
with open(tmp, "w") as fh:
    json.dump(data, fh, indent=2)
    fh.write("\n")
os.replace(tmp, settings)
for cmd in added:
    print(f"  wired PreToolUse Bash hook: {cmd}")
PY
fi
say "  NOTE: the third guard, \`pr-review-report require-qa-block\`, is left to you."
say "        It is the flake-built BINARY, so it needs a GC-rooted path on this box:"
say "          nix profile install $INSTALL_DIR#pr-review-report"
say "        then add { \"type\": \"command\", \"command\": \"pr-review-report require-qa-block\" }"
say "        to the same PreToolUse Bash matcher. A \`nix build\` store path is NOT"
say "        GC-rooted and the hook dies at the next garbage collection."

# ----------------------------------------------------------------- paused ---
# BEFORE the crontab, always. The crontab is what makes the box tick, and a box
# that starts producing the moment it is installed is a second producer over the
# same ORGS — which opens DUPLICATE PRs for the same issues.
step "kill switches (the pipeline installs PAUSED)"
for flag in DISABLED review-DISABLED; do
  if [ -e "$INSTALL_DIR/$flag" ]; then
    did "$INSTALL_DIR/$flag already present"
  elif [ "$DRY_RUN" -eq 1 ]; then
    would "touch $INSTALL_DIR/$flag"
  else
    : >"$INSTALL_DIR/$flag"
    did "wrote $INSTALL_DIR/$flag"
  fi
done

# --------------------------------------------------------------- crontab ----
step "crontab"
# Derived, never baked: `dirname $(command -v nix)` is right for a single-user
# profile and for a multi-user install alike, and it is the only thing cron needs
# to find — everything a run then executes comes from the flake closure.
NIX_BIN="$(dirname "$(command -v nix)")"
CRON_PATH="$NIX_BIN:/usr/bin:/bin"
FLAKE_REF="git+file://$INSTALL_DIR"
BEGIN_MARK="# BEGIN issue-pr-cron ($INSTALL_DIR)"
END_MARK="# END issue-pr-cron ($INSTALL_DIR)"

# `git+file:` and not `path:`. A `path:` ref copies the working directory
# verbatim into the store on every evaluation, and the install dir accumulates
# gitignored work clones and traces (~5GB against ~1MB of tracked files). CI
# asserts the two refs produce identical derivations, so the cheap one is safe.
cron_block() {
  printf '%s\n' "$BEGIN_MARK"
  printf '%s\n' "# Producer and vetter interleave on odd hours; both no-op while their kill switch is present."
  printf '0 1,5,9,13,17,21 * * * PATH=%s CRON_DIR=%s nix run %s#campaign-run\n' "$CRON_PATH" "$INSTALL_DIR" "$FLAKE_REF"
  printf '0 3,7,11,15,19,23 * * * PATH=%s CRON_DIR=%s nix run %s#review-run\n' "$CRON_PATH" "$INSTALL_DIR" "$FLAKE_REF"
  printf '%s\n' "# Disk sweep at midnight, the one run-free gap. It must name EVERY clone root:"
  printf '%s\n' "# clones land in WORK_DIR, and vet-* checkouts accumulate in the install dir."
  printf '0 0 * * * PATH=%s nix run %s#pr-review-report -- gc %s %s >> %s/gc.log 2>&1\n' "$CRON_PATH" "$FLAKE_REF" "$WORK_DIR" "$INSTALL_DIR" "$INSTALL_DIR"
  printf '%s\n' "# Hourly: publishes human-queue.json, its history, and metrics/runs.jsonl to main."
  printf '30 * * * * PATH=%s CRON_DIR=%s nix run %s#refresh-human-queue >> %s/refresh-human-queue.log 2>&1\n' "$CRON_PATH" "$INSTALL_DIR" "$FLAKE_REF" "$INSTALL_DIR"
  printf '%s\n' "# Design-lane doctor, daily, just ahead of the 05:00 producer tick that consumes it."
  printf '0 4 * * * PATH=%s CRON_DIR=%s nix run %s#design-doctor >> %s/design-doctor.log 2>&1\n' "$CRON_PATH" "$INSTALL_DIR" "$FLAKE_REF" "$INSTALL_DIR"
  printf '%s\n' "$END_MARK"
}

existing_crontab="$(crontab -l 2>/dev/null || true)"
# Blank lines are dropped so that "already installed" is a string comparison
# rather than a diff: a crontab is a machine file and a blank line carries
# nothing. Every non-blank line outside the marker block is preserved verbatim.
strip_block() {
  awk -v b="$BEGIN_MARK" -v e="$END_MARK" '
    $0 == b { skip = 1; next }
    $0 == e { skip = 0; next }
    !skip && $0 !~ /^[[:space:]]*$/ { print }
  '
}
stripped=""
[ -z "$existing_crontab" ] || stripped="$(printf '%s\n' "$existing_crontab" | strip_block)"
# An unmanaged line naming this install dir is a second schedule for the same
# pipeline. Splicing beside it would double every tick, so stop instead.
if [ -n "$stripped" ] && printf '%s\n' "$stripped" | grep -Fq "$FLAKE_REF#"; then
  die "the crontab already has line(s) referencing $FLAKE_REF# outside this script's marker block:
$(printf '%s\n' "$stripped" | grep -F "$FLAKE_REF#" | sed 's/^/  /')
Remove them (or fold them into the block) before re-running — splicing beside them would double every tick."
fi
if [ -n "$stripped" ]; then
  new_crontab="$stripped
$(cron_block)"
else
  new_crontab="$(cron_block)"
fi
if [ "$(printf '%s\n' "$existing_crontab" | sed '/^[[:space:]]*$/d')" = "$new_crontab" ]; then
  did "crontab already carries this block, unchanged"
elif [ "$DRY_RUN" -eq 1 ]; then
  would "install this crontab:"
  printf '%s\n' "$new_crontab" | sed 's/^/      /'
else
  printf '%s\n' "$new_crontab" | crontab - || die "crontab install failed"
  did "spliced the issue-pr-cron block into the crontab (other lines untouched)"
fi

# --------------------------------------------------------------- verify -----
step "verify"
if [ "$DRY_RUN" -eq 1 ]; then
  would "run, from the crontab's exact environment:"
  say "      env -i HOME=$HOME USER=${USER:-$(id -un)} LOGNAME=${USER:-$(id -un)} PATH=$CRON_PATH \\"
  say "        nix run \"$FLAKE_REF#pr-review-report\" -- --help"
else
  # The crontab's environment and nothing else — a tool that resolves only
  # because an interactive shell had it is a tool the 01:00 tick does not have.
  # `--help` and not `preflight`: poppler/node/chromium live in the RUNNERS'
  # closures, not this binary's, so preflight would fail for the wrong reason.
  if env -i \
    HOME="$HOME" \
    USER="${USER:-$(id -un)}" \
    LOGNAME="${USER:-$(id -un)}" \
    PATH="$CRON_PATH" \
    nix run "$FLAKE_REF#pr-review-report" -- --help >/dev/null; then
    did "the flake resolves and pr-review-report runs from the crontab's PATH"
  else
    die "could not run pr-review-report from the crontab's PATH. The schedule is installed but would fail every tick — fix this before removing the kill switches."
  fi
fi

# --------------------------------------------------------------- custody ----
cat <<CUSTODY

== two steps this script will not take: key custody

1. Log the dedicated Claude subscription in:

     claude

   The pace gate reads /api/oauth/usage with this credential and FAILS CLOSED
   (#273): with no credential it PAUSES, and both crons pause with it — silently,
   until someone reads the log. Check it after logging in:

     nix run $FLAKE_REF#pr-review-report -- usage-gate

2. Log in a GitHub identity with push rights to every org in ORGS:

     gh auth login

== the cutover — do this in order

The pipeline is installed PAUSED. Two producers over the same ORGS open
DUPLICATE PRs for the same issues, so on the OLD box, FIRST:

  touch <old-install-dir>/DISABLED <old-install-dir>/review-DISABLED
  ls <old-install-dir>/campaign.lock <old-install-dir>/review.lock   # nothing in flight
  crontab -e                                                         # remove its lines

Only then, here:

  rm $INSTALL_DIR/DISABLED $INSTALL_DIR/review-DISABLED

CUSTODY
