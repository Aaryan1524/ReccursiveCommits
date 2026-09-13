#!/usr/bin/env bash
# Proves the daemon publishes a scheduled unit on its own, with no release command issued.
#
# This is the claim Phase 5 rests on: a schedule is only meaningful if something acts on it
# without a person present. The scenario never calls `release publish` — it schedules a unit for a
# time already in the past and waits for the daemon's own maintenance pass to notice and act.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase5.XXXXXX")"
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

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Autonomous release fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
remote_before="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000000501"
task_id="task_00000000-0000-4000-8000-000000000502"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Publish without anyone asking",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Add the scheduled change","dependencies":{},"acceptance_checks":[{"id":"present","description":"the change is published"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace"
printf 'published by the service itself\n' >"$workspace_path/autonomous.txt"
package_id="$(cli package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
[[ "$package_id" == package_* ]] || fail "package capture did not return an ID"

unit_id="$(cli release create-unit "$feature_id" --revision 1 --task "$task_id" | json_field unit_id)"
[[ "$unit_id" == unit_* ]] || fail "release unit creation did not return an ID"

# Every minute of every day is permitted, so the selection lands almost immediately rather than
# waiting for a window the test would have to sit through.
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

# Bring the selection forward so it is due now. From here on nothing issues a release command:
# whatever happens next is the daemon acting on its own schedule.
cli schedule release-now "$unit_id" >/dev/null
cli schedule due --concurrency-limit 10 | grep -q "$unit_id" || \
  fail "the unit did not become due after release-now"

# The maintenance pass runs on its own interval; wait for it rather than prompting it.
published=''
for _ in {1..90}; do
  if [[ "$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)" != "$remote_before" ]]; then
    published=yes
    break
  fi
  sleep 2
done
[[ -n "$published" ]] || \
  fail "the daemon did not publish the due unit on its own within 180 seconds"

remote_after="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"
git -C "$fixture_root/remote.git" merge-base --is-ancestor "$remote_before" "$remote_after" || \
  fail "the autonomous release did not build on the existing history"

# The published commit carries the work, the plan's task name, and the checkout's own identity.
git clone --quiet "$fixture_root/remote.git" "$fixture_root/verification"
[[ -f "$fixture_root/verification/autonomous.txt" ]] || \
  fail "the published commit does not contain the captured change"
subject="$(git -C "$fixture_root/verification" log -1 --format=%s)"
[[ "$subject" == "Add the scheduled change" ]] || \
  fail "the commit message was not derived from the plan task: $subject"
author="$(git -C "$fixture_root/verification" log -1 --format='%an <%ae>')"
[[ "$author" == "Scenario Fixture <fixture@example.invalid>" ]] || \
  fail "the commit was not attributed to the checkout's configured identity: $author"

# No attempt is left unresolved, and nothing is still reported as due.
attempts="$(cli release attempts --limit 10)"
grep -q '"status":"published"' <<<"$attempts" || fail "no published attempt was recorded"
cli schedule due --concurrency-limit 10 | grep -q '"units":\[\]' || \
  fail "the unit is still reported as due after being published"

printf 'PASS the daemon published a due unit on its own, with the plan task name and the checkout identity\n'
