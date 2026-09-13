#!/usr/bin/env bash
# Exercises the exact public CLI flow taught by the packaged Codex skill. Codex is the process
# making these calls in a real session; this scenario proves the skill's required commands create
# an owned workspace, capture one named task, and hand it to the daemon without touching the
# user's checkout or guessing from file-watcher state.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase6d.XXXXXX")"
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

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

# This is the same project-local installation a user performs. The skill is an instruction
# package, while all behavior below goes through the public CLI it documents.
codex_project="$fixture_root/codex-project"
mkdir -p "$codex_project/.codex/skills"
cp -R "$project_root/integrations/codex/reccursive" "$codex_project/.codex/skills/reccursive"
[[ -f "$codex_project/.codex/skills/reccursive/SKILL.md" ]] || fail "the Codex skill was not installed"

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Codex integration fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
checkout_head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
feature_id="feature_00000000-0000-4000-8000-000000000d01"
task_id="task_00000000-0000-4000-8000-000000000d02"

cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Codex handoff captures one real task boundary",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Add the Codex-owned change","dependencies":{},"acceptance_checks":[{"id":"present","description":"file is present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

cat >"$fixture_root/schedule-policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 20},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null

# The skill's start command returns the only directory an agent is allowed to modify.
workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
[[ "$workspace_path" != "$fixture_root/repository" ]] || fail "Codex was handed the user checkout"
printf 'owned by the Codex workspace\n' >"$workspace_path/codex-change.txt"

# The skill's completion command captures, groups, and schedules the named task. No direct Git
# commit or push is used, and its result is read through feature status.
submission="$(cli task submit "$feature_id" --revision 1 --task "$task_id")"
grep -q '"created":true' <<<"$submission" || fail "the Codex handoff did not create a submission"
status="$(cli feature status "$feature_id" --revision 1)"
grep -q '"status":"scheduled"' <<<"$status" || fail "the submitted task was not scheduled"
grep -q '"package_id":"package_' <<<"$status" || fail "feature status did not expose the captured package"
[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$checkout_head_before" ]] || \
  fail "the Codex handoff changed the user's checkout"
[[ -z "$(git -C "$fixture_root/repository" status --porcelain)" ]] || \
  fail "the Codex handoff dirtied the user's checkout"

printf 'PASS packaged Codex skill flow captured and scheduled one named task from an owned workspace\n'
