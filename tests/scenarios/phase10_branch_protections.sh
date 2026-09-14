#!/usr/bin/env bash
# Proves a target branch that refuses direct pushes is respected rather than worked around.
#
# The failure this guards against is a bypass or a false completion: a protected branch must leave
# the work visibly blocked, with the remote untouched and the task not reported as published. It
# also proves the block is actionable — a blocked unit whose owner is told nothing is the same
# problem in a politer form.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase10-protect.XXXXXX")"
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

json_field() {
  local field="$1"
  sed -E "s/.*\\\"$field\\\":\\\"([^\\\"]+)\\\".*/\\1/"
}

cli() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json "$@"
}

plain() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" "$@"
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Protected branch fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
main_before="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"

# A real refusal from the remote itself, in the words a host actually uses. Nothing in the service
# is told the branch is protected: it has to learn that from the push being rejected, which is
# exactly how it learns it from GitHub.
cat >"$fixture_root/remote.git/hooks/pre-receive" <<'HOOK'
#!/bin/sh
while read -r _old _new ref; do
  if [ "$ref" = "refs/heads/main" ]; then
    echo "remote: error: GH006: Protected branch update failed for refs/heads/main." >&2
    echo "remote: error: Required status checks must pass before merging." >&2
    exit 1
  fi
done
exit 0
HOOK
chmod +x "$fixture_root/remote.git/hooks/pre-receive"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000001101"
task_id="task_00000000-0000-4000-8000-000000001102"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Publish to a branch that refuses direct pushes",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Add the refused change","dependencies":{},"acceptance_checks":[{"id":"present","description":"the change is ready"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'refused by a protected branch\n' >"$workspace_path/refused.txt"
package_id="$(cli package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
unit_id="$(cli release create-unit "$feature_id" --revision 1 --task "$task_id" | json_field unit_id)"

cat >"$fixture_root/schedule-policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 20},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "catch_up", "max_releases": 5}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null
cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 >/dev/null
cli schedule release-now "$unit_id" >/dev/null

# --- The refusal is recognised for what it is. ---
blocked=''
for _ in {1..90}; do
  attempts="$(cli release attempts --limit 10)"
  if grep -q '"failure_classification":"branch_rule"' <<<"$attempts"; then
    blocked=yes
    break
  fi
  sleep 2
done
[[ -n "$blocked" ]] || \
  fail "a protected-branch refusal was not classified as branch_rule within 180 seconds: ${attempts:-none}"

# --- No bypass, and no false completion. ---
[[ "$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)" == "$main_before" ]] || \
  fail "the protected target moved despite refusing the push"

status_json="$(cli feature status "$feature_id" --revision 1)"
grep -q '"status":"published"' <<<"$status_json" && \
  fail "a task refused by a protected branch is reported as published"
grep -q '"status":"blocked"' <<<"$status_json" || \
  fail "a task refused by a protected branch is not reported as blocked: $status_json"
grep -q '"published_to":\[\]' <<<"$status_json" || \
  fail "a task that never reached a branch reports a publication: $status_json"

# --- The block is actionable rather than merely honest. ---
attempt_id="$(cli release attempts --limit 1 | json_field attempt_id)"
attempt_text="$(plain release attempt "$attempt_id")"
grep -q "branch_rule" <<<"$attempt_text" || \
  fail "the attempt does not report why it stopped: $attempt_text"
grep -q "Next:" <<<"$attempt_text" || \
  fail "a blocked attempt leaves its owner with nothing to do: $attempt_text"
grep -q -- "--mode immediate" <<<"$attempt_text" || \
  fail "the guidance does not name a branch the work can actually reach: $attempt_text"
grep -q "refuses direct pushes" <<<"$attempt_text" || \
  fail "the guidance does not explain what the remote refused: $attempt_text"

# --- And it is findable without knowing which attempt to look at. ---
attention="$(cli queue status --needs-attention)"
grep -q "$unit_id" <<<"$attention" || \
  fail "a unit blocked by a protected branch is not listed as needing attention: $attention"

# --- A branch the rules permit is still publishable, so the advice is real. ---
git -C "$fixture_root/repository" push --quiet origin main:refs/heads/development || \
  fail "the fixture's own hook refuses branches it was not meant to protect"

printf 'PASS a protected target blocks visibly, moves nothing, claims nothing, and says what to do instead\n'
