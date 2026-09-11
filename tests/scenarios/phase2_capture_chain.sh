#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase2.XXXXXX")"
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
  [[ -f "$fixture_root/daemon.log" ]] && sed -n '1,160p' "$fixture_root/daemon.log" >&2
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

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Capture chain fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
printf 'This user change must remain untouched.\n' >"$fixture_root/repository/local-notes.txt"
source_head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"
source_status_before="$(git -C "$fixture_root/repository" status --porcelain=v1)"

start_daemon
enrollment_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json repository add "$fixture_root/repository")"
repository_id="$(json_field id <<<"$enrollment_json")"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000000101"
task_one="task_00000000-0000-4000-8000-000000000102"
task_two="task_00000000-0000-4000-8000-000000000103"
task_three="task_00000000-0000-4000-8000-000000000104"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Capture three ordered units without touching the user checkout",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_one","name":"First unit","dependencies":{},"acceptance_checks":[{"id":"first","description":"first unit is captured"}]},
      {"id":"$task_two","name":"Second unit","dependencies":{"$task_one":"captured"},"acceptance_checks":[{"id":"second","description":"second unit is captured"}]},
      {"id":"$task_three","name":"Third unit","dependencies":{"$task_two":"captured"},"acceptance_checks":[{"id":"third","description":"third unit is captured"}]}
    ]
  }]
}
JSON
"$binary_dir/reccursive" --state-dir "$fixture_root/state" --json plan import "$fixture_root/feature-plan.json" >/dev/null
workspace_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json workspace create "$feature_id" --revision 1)"
workspace_path="$(json_field path <<<"$workspace_json")"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace path"

first_json="$(capture_unit "$task_one" first.txt 'first unit')"
first_id="$(json_field package_id <<<"$first_json")"
first_base="$(json_field base_tree <<<"$first_json")"
first_result="$(json_field result_tree <<<"$first_json")"
[[ "$first_id" == package_* ]] || fail "first capture did not return a package ID"

# The first package must remain usable after a full daemon restart before later units are built.
stop_daemon
start_daemon
"$binary_dir/reccursive" --state-dir "$fixture_root/state" --json package show "$first_id" >/dev/null

second_json="$(capture_unit "$task_two" second.txt 'second unit')"
second_id="$(json_field package_id <<<"$second_json")"
second_base="$(json_field base_tree <<<"$second_json")"
second_result="$(json_field result_tree <<<"$second_json")"
third_json="$(capture_unit "$task_three" third.txt 'third unit')"
third_id="$(json_field package_id <<<"$third_json")"
third_base="$(json_field base_tree <<<"$third_json")"
third_result="$(json_field result_tree <<<"$third_json")"

[[ "$first_result" == "$second_base" ]] || fail "second package did not start from the first result tree"
[[ "$second_result" == "$third_base" ]] || fail "third package did not start from the second result tree"
[[ "$first_result" != "$second_result" && "$second_result" != "$third_result" ]] || \
  fail "ordered packages must have distinct result trees"
grep -Eq "\"parent_package_id\"[[:space:]]*:[[:space:]]*\"$first_id\"" "$fixture_root/state/packages/$second_id/revision-1/manifest.json" || \
  fail "second package did not name the first package as its parent"
grep -Eq "\"parent_package_id\"[[:space:]]*:[[:space:]]*\"$second_id\"" "$fixture_root/state/packages/$third_id/revision-1/manifest.json" || \
  fail "third package did not name the second package as its parent"

cancel_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json task cancel "$feature_id" \
  --revision 1 --task "$task_two" --message "middle unit was replaced")"
grep -q '"type":"task_cancelled"' <<<"$cancel_json" || fail "task cancellation did not return its stable response type"
grep -Fq "\"blocked_dependents\":[\"$task_three\"]" <<<"$cancel_json" || \
  fail "cancelling the middle unit did not block its dependent third unit"

[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$source_head_before" ]] || \
  fail "capture chain moved the user checkout HEAD"
[[ "$(git -C "$fixture_root/repository" status --porcelain=v1)" == "$source_status_before" ]] || \
  fail "capture chain changed the user checkout"
queue_audit_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json queue audit)"
grep -q '"verified_package_count":3' <<<"$queue_audit_json" || fail "queue audit did not retain three verified packages"

printf 'PASS three ordered captures, restart survival, untouched source checkout, and middle-unit cancellation\n'
