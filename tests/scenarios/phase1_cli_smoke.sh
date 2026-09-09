#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase1.XXXXXX")"
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
  if [[ -n "${fixture_root:-}" && -d "$fixture_root" ]]; then
    chmod -R u+w "$fixture_root" 2>/dev/null || true
    rm -rf -- "$fixture_root"
  fi
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  if [[ -f "$fixture_root/daemon.log" ]]; then
    sed -n '1,120p' "$fixture_root/daemon.log" >&2
  fi
  exit 1
}

start_daemon() {
  "$binary_dir/reccursive-daemon" --state-dir "$fixture_root/state" \
    >"$fixture_root/daemon.log" 2>&1 &
  daemon_pid=$!
  for _ in {1..50}; do
    if "$binary_dir/reccursive" --state-dir "$fixture_root/state" \
      --json status >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$daemon_pid" 2>/dev/null || fail "daemon exited during startup"
    sleep 0.05
  done
  fail "daemon socket was not ready in time"
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "CLI Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
"$binary_dir/reccursive" --state-dir "$fixture_root/state" doctor >/dev/null
enrollment_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json repository add "$fixture_root/repository")"
grep -q '"type":"repository_enrolled"' <<<"$enrollment_json" || \
  fail "repository enrollment did not return the expected JSON payload"
repository_id="$(sed -E 's/.*"id":"(repo_[^"]+)".*/\1/' <<<"$enrollment_json")"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "feature_00000000-0000-4000-8000-000000000001",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Exercise durable plan import",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [{
      "id": "task_00000000-0000-4000-8000-000000000002",
      "name": "Import the plan",
      "dependencies": {},
      "acceptance_checks": [{
        "id": "round_trip",
        "description": "The imported plan survives restart"
      }]
    }]
  }]
}
JSON
plan_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json plan import "$fixture_root/feature-plan.json")"
grep -q '"type":"plan_imported"' <<<"$plan_json" || \
  fail "plan import did not return the expected JSON payload"

printf 'Local prerequisite\n' >>"$fixture_root/repository/README.md"
source_head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"
source_status_before="$(git -C "$fixture_root/repository" status --porcelain=v1)"
workspace_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json workspace create feature_00000000-0000-4000-8000-000000000001 \
  --include README.md)"
grep -q '"type":"workspace_created"' <<<"$workspace_json" || \
  fail "workspace creation did not return the expected JSON payload"
grep -q '"path":"README.md","state":"present"' <<<"$workspace_json" || \
  fail "workspace did not record the explicit prerequisite"
[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$source_head_before" ]] || \
  fail "workspace creation moved the source checkout HEAD"
[[ "$(git -C "$fixture_root/repository" status --porcelain=v1)" == "$source_status_before" ]] || \
  fail "workspace creation changed the source checkout"

list_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json repository list)"
grep -q '"type":"repositories"' <<<"$list_json" || \
  fail "repository list did not return the expected JSON payload"
grep -q '"policy_revision":1' <<<"$list_json" || \
  fail "repository policy revision was not persisted"

set +e
duplicate_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json repository add "$fixture_root/repository" 2>&1)"
duplicate_exit=$?
set -e
[[ $duplicate_exit -eq 12 ]] || fail "duplicate enrollment did not use conflict exit code 12"
grep -q '"code":"conflict"' <<<"$duplicate_json" || \
  fail "duplicate enrollment did not return the conflict error code"

stop_daemon
start_daemon
status_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json status)"
grep -q '"repository_count":1' <<<"$status_json" || \
  fail "repository state did not survive daemon restart"
stored_plan_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json \
  plan show feature_00000000-0000-4000-8000-000000000001)"
grep -q '"goal":"Exercise durable plan import"' <<<"$stored_plan_json" || \
  fail "feature plan did not survive daemon restart"
history_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json \
  plan history feature_00000000-0000-4000-8000-000000000001)"
grep -q '"type":"plan_history"' <<<"$history_json" || \
  fail "plan history did not return the expected JSON payload"
workspace_after_restart="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  --json workspace show feature_00000000-0000-4000-8000-000000000001 --revision 1)"
grep -q '"type":"workspace"' <<<"$workspace_after_restart" || \
  fail "workspace ownership did not survive daemon restart"

logs_json="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" --json logs --limit 20)"
grep -q '"type":"events"' <<<"$logs_json" || \
  fail "logs did not return the expected JSON payload"
grep -q '"kind":"api.request_succeeded"' <<<"$logs_json" || \
  fail "successful API calls were not recorded"
grep -q '"kind":"api.request_failed"' <<<"$logs_json" || \
  fail "failed API calls were not recorded"
grep -q '"reason_code":"conflict"' <<<"$logs_json" || \
  fail "failed API calls did not retain a stable reason code"
grep -q '"request_id":"request_' <<<"$logs_json" || \
  fail "diagnostic events were not correlated by request ID"

printf 'PASS foundation CLI, plan import, isolated workspace ownership, diagnostics, and restart persistence\n'
