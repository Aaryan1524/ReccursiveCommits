#!/usr/bin/env bash
# Proves a new repository can be set up end to end from the terminal, and that a non-interactive
# setup refuses missing values instead of waiting for someone to type them.
#
# Only the non-interactive half is provable here, and deliberately so: CI has no terminal, which is
# exactly the condition the refusal exists for. The guided path is verified by hand — see the
# evidence recorded for P8-T02 — because a scripted stand-in for a terminal would prove the state
# machine while leaving the real prompting path covered by nothing.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8.XXXXXX")"
daemon_pid=''

stop_daemon() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=''
  fi
}

cleanup() {
  stop_daemon
  [[ -n "${fixture_root:-}" && -d "$fixture_root" ]] || return 0
  chmod -R u+w "$fixture_root" 2>/dev/null || true
  rm -rf -- "$fixture_root"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  [[ -f "$fixture_root/daemon.log" ]] && sed -n '1,200p' "$fixture_root/daemon.log" >&2
  exit 1
}

start_daemon() {
  "$binary_dir/reccursive-daemon" --state-dir "$fixture_root/state" \
    >"$fixture_root/daemon.log" 2>&1 &
  daemon_pid=$!
  for _ in {1..80}; do
    if "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json status >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$daemon_pid" 2>/dev/null || fail "daemon exited during startup"
    sleep 0.05
  done
  fail "daemon socket was not ready in time"
}

cli() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json "$@"
}

# How many repositories the installation knows about. Setup must never leave a partial enrollment
# behind when it refuses.
enrolled_count() {
  cli repository list 2>/dev/null | grep -o '"id":"repo_' | wc -l | tr -d ' '
}

new_repository() {
  local name="$1" identity="$2"
  git init --quiet --bare --initial-branch=main "$fixture_root/$name-remote.git"
  git init --quiet --initial-branch=main "$fixture_root/$name"
  if [[ "$identity" == "with-identity" ]]; then
    git -C "$fixture_root/$name" config user.name "Scenario Fixture"
    git -C "$fixture_root/$name" config user.email "fixture@example.invalid"
  else
    # An empty value is how a real misconfiguration looks, and it must be treated as absent.
    git -C "$fixture_root/$name" config user.name ""
    git -C "$fixture_root/$name" config user.email ""
  fi
  printf '# %s\n' "$name" >"$fixture_root/$name/README.md"
  git -C "$fixture_root/$name" -c user.name=Seed -c user.email=seed@example.invalid \
    add README.md
  git -C "$fixture_root/$name" -c user.name=Seed -c user.email=seed@example.invalid \
    commit --quiet -m "Initialize $name"
  git -C "$fixture_root/$name" remote add origin "$fixture_root/$name-remote.git"
  git -C "$fixture_root/$name" push --quiet -u origin main
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

start_daemon

# ---- The whole thing in one non-interactive command ----

new_repository primary with-identity
setup_output="$(cli setup "$fixture_root/primary" --non-interactive --timezone UTC)"
grep -q '"type":"setup_complete"' <<<"$setup_output" || \
  fail "setup did not report completing: $setup_output"
grep -q '"schedule_activated":true' <<<"$setup_output" || \
  fail "setup did not activate a schedule: $setup_output"
repository_id="$(sed -E 's/.*"repository_id":"([^"]+)".*/\1/' <<<"$setup_output")"
[[ "$repository_id" == repo_* ]] || fail "setup did not return a repository id: $setup_output"

# Everything it claimed is durable and readable through the ordinary commands.
cli repository list | grep -q "$repository_id" || fail "the repository was not enrolled"
policy="$(cli schedule show-policy "$repository_id")"
grep -q '"timezone":"UTC"' <<<"$policy" || fail "the default schedule policy was not activated: $policy"
cli diagnose "$repository_id" >/dev/null || fail "the enrolled repository cannot be diagnosed"

# ---- Setup never publishes anything by itself ----

[[ "$(cli release attempts --limit 5 | grep -o '"attempt_id"' | wc -l | tr -d ' ')" == "0" ]] || \
  fail "setup started a publication attempt"

# ---- Repeating setup does not enroll a second time ----

before_repeat="$(enrolled_count)"
cli setup "$fixture_root/primary" --non-interactive --timezone UTC \
  >"$fixture_root/repeat.json" 2>&1 && fail "setting up an already-enrolled repository was accepted"
grep -q '"code":"conflict"' "$fixture_root/repeat.json" || \
  fail "a repeated setup was not refused as a conflict: $(cat "$fixture_root/repeat.json")"
[[ "$(enrolled_count)" == "$before_repeat" ]] || \
  fail "a refused repeat changed how many repositories are enrolled"

# ---- Refusals: each names what is missing, and none of them enrol anything ----

refuses() {
  local description="$1" expected="$2"
  shift 2
  local before after
  before="$(enrolled_count)"
  if cli "$@" >"$fixture_root/refusal.json" 2>&1; then
    fail "$description was accepted: $(cat "$fixture_root/refusal.json")"
  fi
  grep -q "$expected" "$fixture_root/refusal.json" || \
    fail "$description did not name what is missing ($expected): $(cat "$fixture_root/refusal.json")"
  after="$(enrolled_count)"
  [[ "$before" == "$after" ]] || fail "$description enrolled something while refusing"
}

# Immediate mode publishes to a development branch first, so it cannot be inferred.
new_repository immediate with-identity
refuses "immediate mode without a development branch" "development-target" \
  setup "$fixture_root/immediate" --non-interactive --mode immediate

# A checkout with no committer identity can be enrolled and scheduled and would still never
# publish, because the release pass skips a unit it cannot attribute — silently. Setup refuses
# instead, while there is someone to tell.
new_repository anonymous no-identity
refuses "a checkout with no committer identity" "committer identity" \
  setup "$fixture_root/anonymous" --non-interactive

refuses "a path that is not a Git repository" "not inside a Git repository" \
  setup "$fixture_root" --non-interactive

# ---- Without a terminal and without the flag, setup refuses rather than waiting ----

# This is the exit criterion itself. CI has no terminal, so the condition is real here: a prompt
# would hang until the job timed out. The refusal must be immediate.
started="$(date +%s)"
if cli setup "$fixture_root/immediate" >"$fixture_root/no-tty.json" 2>&1 </dev/null; then
  fail "setup ran to completion with no terminal and no --non-interactive"
fi
elapsed=$(( $(date +%s) - started ))
[[ "$elapsed" -lt 20 ]] || fail "setup took ${elapsed}s to refuse; it was waiting for input"
grep -q 'non-interactive' "$fixture_root/no-tty.json" || \
  fail "the refusal did not say how to run without a terminal: $(cat "$fixture_root/no-tty.json")"

# ---- An explicit policy document is used instead of the built-in default ----

cat >"$fixture_root/policy.json" <<'JSON'
{
  "timezone": "Asia/Tokyo",
  "allowed_days": ["saturday","sunday"],
  "windows": [{"start":{"hour":10,"minute":0},"end":{"hour":12,"minute":0}}],
  "daily_releases": {"minimum": 1, "maximum": 2},
  "minimum_spacing_minutes": 30,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
new_repository weekend with-identity
weekend_output="$(cli setup "$fixture_root/weekend" --non-interactive --schedule "$fixture_root/policy.json")"
weekend_id="$(sed -E 's/.*"repository_id":"([^"]+)".*/\1/' <<<"$weekend_output")"
weekend_policy="$(cli schedule show-policy "$weekend_id")"
grep -q '"timezone":"Asia/Tokyo"' <<<"$weekend_policy" || \
  fail "the supplied policy was not the one activated: $weekend_policy"

# ---- Enrolling without any schedule is allowed and says so ----

new_repository unscheduled with-identity
unscheduled_output="$(cli setup "$fixture_root/unscheduled" --non-interactive --no-schedule)"
grep -q '"schedule_activated":false' <<<"$unscheduled_output" || \
  fail "--no-schedule still activated a policy: $unscheduled_output"

printf 'PASS setup enrolled, scheduled, and refused every missing value without enrolling anything\n'
