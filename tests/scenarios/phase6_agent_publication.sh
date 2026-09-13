#!/usr/bin/env bash
# Proves the whole supported-agent path: a Codex-owned workspace produces three separately
# submitted units, and the daemon verifies and publishes all three with no release command,
# schedule override, or per-commit action after handoff.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase6e.XXXXXX")"
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
  if [[ -d "$fixture_root/remote.git" ]]; then
    printf '%s\n' 'Published remote history:' >&2
    git -C "$fixture_root/remote.git" log --format='%H%x09%s' refs/heads/main >&2 || true
  fi
  if [[ -S "$fixture_root/state/service.sock" ]]; then
    printf '%s\n' 'Durable release attempts:' >&2
    cli release attempts --limit 20 >&2 || true
    printf '%s\n' 'Feature status:' >&2
    cli feature status "${feature_id:-}" --revision 1 >&2 || true
    printf '%s\n' 'Queue audit:' >&2
    cli queue audit >&2 || true
  fi
  if [[ -d "$fixture_root/remote.git" ]]; then
    printf '%s\n' 'Each published commit and what it changed:' >&2
    git -C "$fixture_root/remote.git" log --format='--- %H %s' --name-status refs/heads/main >&2 || true
  fi
  if [[ -d "$fixture_root/state/packages" ]]; then
    printf '%s\n' 'Captured packages and their parents:' >&2
    for manifest in "$fixture_root"/state/packages/*/revision-*/manifest.json; do
      [[ -f "$manifest" ]] || continue
      printf '%s\n' "$manifest" >&2
      tr ',' '\n' <"$manifest" | grep -E 'package_id|parent_package_id|base_tree|result_tree|task_ids' >&2 || true
    done
  fi
  [[ -f "$fixture_root/daemon.log" ]] && sed -n '1,240p' "$fixture_root/daemon.log" >&2
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

# Pick a five-minute future window in a timezone that will not cross local midnight. That gives
# three one-minute-spaced submissions a short, real policy window without relying on a fake clock.
schedule_window() {
  ruby <<'RUBY'
zones = %w[UTC America/New_York Asia/Tokyo]
zones.each do |zone|
  ENV['TZ'] = zone
  now = Time.now
  start = Time.at((now.to_i / 60 + 1) * 60).getlocal
  finish = start + 5 * 60
  next unless start.strftime('%F') == finish.strftime('%F')

  puts [zone, start.hour, start.min, finish.hour, finish.min].join(' ')
  exit
end
abort 'no same-day scheduling timezone available'
RUBY
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

codex_project="$fixture_root/codex-project"
mkdir -p "$codex_project/.codex/skills"
cp -R "$project_root/integrations/codex/reccursive" "$codex_project/.codex/skills/reccursive"
[[ -f "$codex_project/.codex/skills/reccursive/SKILL.md" ]] || fail "the Codex skill was not installed"

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Agent-to-publication fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
checkout_head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"
remote_before="$checkout_head_before"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
feature_id="feature_00000000-0000-4000-8000-000000000e01"
task_one="task_00000000-0000-4000-8000-000000000e02"
task_two="task_00000000-0000-4000-8000-000000000e03"
task_three="task_00000000-0000-4000-8000-000000000e04"

cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Publish three separately completed Codex units",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_one","name":"Add first agent unit","dependencies":{},"acceptance_checks":[{"id":"one","description":"first file is present"}]},
      {"id":"$task_two","name":"Add second agent unit","dependencies":{},"acceptance_checks":[{"id":"two","description":"second file is present"}]},
      {"id":"$task_three","name":"Add third agent unit","dependencies":{},"acceptance_checks":[{"id":"three","description":"third file is present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

read -r timezone start_hour start_minute end_hour end_minute <<<"$(schedule_window)"
cat >"$fixture_root/schedule-policy.json" <<JSON
{
  "timezone": "$timezone",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":$start_hour,"minute":$start_minute},"end":{"hour":$end_hour,"minute":$end_minute}}],
  "daily_releases": {"minimum": 3, "maximum": 3},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "catch_up", "max_releases": 3}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null

# The following is the concrete path the Codex skill prescribes: the daemon names the workspace,
# the agent writes one completed task at a time, and every task is handed off only by submit.
workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
[[ "$workspace_path" != "$fixture_root/repository" ]] || fail "the agent was handed the user checkout"

printf 'first complete unit\n' >"$workspace_path/first.txt"
first_submission="$(cli task submit "$feature_id" --revision 1 --task "$task_one")"
grep -q '"created":true' <<<"$first_submission" || fail "the first agent unit was not submitted"

printf 'second complete unit\n' >"$workspace_path/second.txt"
second_submission="$(cli task submit "$feature_id" --revision 1 --task "$task_two")"
grep -q '"created":true' <<<"$second_submission" || fail "the second agent unit was not submitted"

printf 'third complete unit\n' >"$workspace_path/third.txt"
third_submission="$(cli task submit "$feature_id" --revision 1 --task "$task_three")"
grep -q '"created":true' <<<"$third_submission" || fail "the third agent unit was not submitted"

# The agent has finished. There is deliberately no release command, no release-now command, and
# no schedule mutation below: only the daemon's maintenance pass may publish these three units.
published=''
for _ in {1..210}; do
  attempts="$(cli release attempts --limit 10)"
  if [[ "$(grep -o '"status":"published"' <<<"$attempts" | wc -l | tr -d ' ')" == "3" ]]; then
    published=yes
    break
  fi
  sleep 2
done
[[ -n "$published" ]] || fail "the daemon did not publish all three submitted units within seven minutes"

remote_after="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"
git -C "$fixture_root/remote.git" merge-base --is-ancestor "$remote_before" "$remote_after" || \
  fail "the published history did not retain the original remote commit"
git clone --quiet "$fixture_root/remote.git" "$fixture_root/verification"
for file in first.txt second.txt third.txt; do
  [[ -f "$fixture_root/verification/$file" ]] || fail "published history is missing $file"
done
for subject in "Add first agent unit" "Add second agent unit" "Add third agent unit"; do
  git -C "$fixture_root/verification" log --format=%s | grep -Fxq "$subject" || \
    fail "published history is missing the plan-derived subject: $subject"
done

status="$(cli feature status "$feature_id" --revision 1)"
[[ "$(grep -o '"status":"published"' <<<"$status" | wc -l | tr -d ' ')" == "3" ]] || \
  fail "feature status did not record all three tasks as published"
[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$checkout_head_before" ]] || \
  fail "the autonomous flow changed the user's checkout"
[[ -z "$(git -C "$fixture_root/repository" status --porcelain)" ]] || \
  fail "the autonomous flow dirtied the user's checkout"

printf 'PASS Codex submitted three completed units and the daemon verified and published all three autonomously\n'
