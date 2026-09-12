#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase3.XXXXXX")"
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
  for _ in {1..50}; do
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

capture_unit() {
  local task_id="$1"
  local file_name="$2"
  local content="$3"
  printf '%s\n' "$content" >"$workspace_path/$file_name"
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json package capture \
    "$feature_id" --revision 1 --task "$task_id"
}

release_unit() {
  local package_id="$1"
  local message="$2"
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json release publish \
    "$package_id" --message "$message" --author-name "Scenario Fixture" \
    --author-email "fixture@example.invalid"
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Release scenario fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
printf 'This user checkout must remain untouched.\n' >"$fixture_root/repository/local-notes.txt"
source_head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"
source_status_before="$(git -C "$fixture_root/repository" status --porcelain=v1)"

start_daemon
enrollment_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json repository add "$fixture_root/repository")"
repository_id="$(json_field id <<<"$enrollment_json")"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000000301"
task_one="task_00000000-0000-4000-8000-000000000302"
task_two="task_00000000-0000-4000-8000-000000000303"
task_three="task_00000000-0000-4000-8000-000000000304"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Release three independently captured units through the local CLI",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_one","name":"First release","dependencies":{},"acceptance_checks":[{"id":"first","description":"first unit publishes"}]},
      {"id":"$task_two","name":"Second release","dependencies":{},"acceptance_checks":[{"id":"second","description":"second unit publishes after restart"}]},
      {"id":"$task_three","name":"Third release","dependencies":{},"acceptance_checks":[{"id":"third","description":"third unit reconciles with a competing push"}]}
    ]
  }]
}
JSON
"$binary_dir/reccursive" --state-dir "$fixture_root/state" --json plan import "$fixture_root/feature-plan.json" >/dev/null
workspace_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json workspace create "$feature_id" --revision 1)"
workspace_path="$(json_field path <<<"$workspace_json")"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace"

first_json="$(capture_unit "$task_one" first.txt 'first unit')"
first_id="$(json_field package_id <<<"$first_json")"
first_release="$(release_unit "$first_id" 'feat: publish first unit')"
grep -q '"type":"release_attempt"' <<<"$first_release" || fail "first release did not return an attempt"
grep -q '"status":"published"' <<<"$first_release" || fail "first release was not published"

second_json="$(capture_unit "$task_two" second.txt 'second unit')"
second_id="$(json_field package_id <<<"$second_json")"

# An abrupt stop after capture must not lose the queued package. The next daemon owns recovery and
# the CLI can continue from durable state without the user doing Git repair work.
stop_daemon
start_daemon
second_release="$(release_unit "$second_id" 'feat: publish second unit after restart')"
grep -q '"status":"published"' <<<"$second_release" || \
  fail "second release did not survive daemon restart: $second_release"

third_json="$(capture_unit "$task_three" third.txt 'third unit')"
third_id="$(json_field package_id <<<"$third_json")"

# Another writer moves the remote after capture. The release worker must reconcile onto that
# commit, never force-push over it.
git clone --quiet "$fixture_root/remote.git" "$fixture_root/competitor"
git -C "$fixture_root/competitor" config user.name "Competing Writer"
git -C "$fixture_root/competitor" config user.email "competing@example.invalid"
printf 'competing change\n' >"$fixture_root/competitor/other.txt"
git -C "$fixture_root/competitor" add other.txt
git -C "$fixture_root/competitor" commit --quiet -m "Competing work"
git -C "$fixture_root/competitor" push --quiet origin main
competing_commit="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"

third_release="$(release_unit "$third_id" 'feat: publish third unit after reconciliation')"
grep -q '"status":"published"' <<<"$third_release" || fail "third release was not published"
third_attempt="$(json_field attempt_id <<<"$third_release")"
attempt_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json release attempt "$third_attempt")"
grep -q '"type":"release_attempt"' <<<"$attempt_json" || fail "attempt inspection did not return a release attempt"
attempts_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json release attempts --limit 10)"
grep -q '"type":"release_attempts"' <<<"$attempts_json" || fail "attempt listing did not return release attempts"

final_head="$(git -C "$fixture_root/remote.git" rev-parse refs/heads/main)"
git -C "$fixture_root/remote.git" merge-base --is-ancestor "$competing_commit" "$final_head" || \
  fail "third release overwrote the competing remote commit"
git clone --quiet "$fixture_root/remote.git" "$fixture_root/verification"
for file in first.txt second.txt third.txt other.txt; do
  [[ -f "$fixture_root/verification/$file" ]] || fail "published target is missing $file"
done

[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$source_head_before" ]] || \
  fail "release flow moved the user checkout HEAD"
[[ "$(git -C "$fixture_root/repository" status --porcelain=v1)" == "$source_status_before" ]] || \
  fail "release flow changed the user checkout"

printf 'PASS three CLI releases, restart survival, competing-push reconciliation, and untouched user checkout\n'
