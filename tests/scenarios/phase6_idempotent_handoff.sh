#!/usr/bin/env bash
# Proves a repeated request carrying the same idempotency key returns the first result rather than
# acting a second time.
#
# This is what makes the service safe for an agent to drive. An agent whose connection drops cannot
# tell a request that never arrived from one that arrived and answered; retrying is the only move it
# has. Everything here goes through the real CLI and the real socket, because the guarantee is about
# what the daemon does with a second request — not about what a library function returns when called
# twice in one process.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase6.XXXXXX")"
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
printf '# Idempotent handoff fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon

# Enrollment, repeated under one key. Without the key the second call is a duplicate-repository
# conflict; with it, the agent gets the identifier it already has.
enroll_key="agent-alpha/enroll/attempt-1"
repository_id="$(cli --idempotency-key "$enroll_key" repository add "$fixture_root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"
repeated_id="$(cli --idempotency-key "$enroll_key" repository add "$fixture_root/repository" | json_field id)"
[[ "$repeated_id" == "$repository_id" ]] || \
  fail "the repeated enrollment returned $repeated_id instead of the original $repository_id"
enrolled_count="$(cli repository list | grep -o '"id":"repo_' | wc -l | tr -d ' ')"
[[ "$enrolled_count" == "1" ]] || \
  fail "the repeated enrollment left $enrolled_count repositories enrolled instead of one"

# The same repeat without a key is not a retry — it is a second request, and is refused as one.
# This is what shows the replay above came from the key rather than from the command being inert.
if cli repository add "$fixture_root/repository" >"$fixture_root/unkeyed.json" 2>&1; then
  fail "an unkeyed duplicate enrollment was accepted"
fi
grep -q '"code":"conflict"' "$fixture_root/unkeyed.json" || \
  fail "an unkeyed duplicate enrollment was not refused as a conflict: $(cat "$fixture_root/unkeyed.json")"

feature_id="feature_00000000-0000-4000-8000-000000000601"
task_id="task_00000000-0000-4000-8000-000000000602"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Survive a dropped connection",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Capture exactly once","dependencies":{},"acceptance_checks":[{"id":"present","description":"one package exists"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'captured once\n' >"$workspace_path/handoff.txt"

# The case that matters most: capture is the step an agent is most likely to retry, and capturing
# twice would otherwise produce two packages of the same work.
capture_key="agent-alpha/capture/$task_id"
package_id="$(cli --idempotency-key "$capture_key" package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
[[ "$package_id" == package_* ]] || fail "package capture did not return an ID"
repeated_package="$(cli --idempotency-key "$capture_key" package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
[[ "$repeated_package" == "$package_id" ]] || \
  fail "the retried capture returned $repeated_package instead of the original $package_id"

# A key reused for a genuinely different request is a client bug, and is reported rather than
# answered with the earlier result.
if cli --idempotency-key "$capture_key" release create-unit "$feature_id" --revision 1 --task "$task_id" \
     >"$fixture_root/mismatch.json" 2>&1; then
  fail "reusing one key for a different request was accepted"
fi
grep -q '"code":"conflict"' "$fixture_root/mismatch.json" || \
  fail "reusing one key for a different request was not reported as a conflict: $(cat "$fixture_root/mismatch.json")"

# A key is scoped to the request it named, so a different key runs the command for real.
unit_id="$(cli --idempotency-key "agent-alpha/unit/$task_id" release create-unit "$feature_id" --revision 1 --task "$task_id" | json_field unit_id)"
[[ "$unit_id" == unit_* ]] || fail "a fresh key did not carry out the release unit creation"

# Keys on queries are accepted and ignored: freezing an answer would make the service lie about
# state that has since moved.
first_status="$(cli --idempotency-key "agent-alpha/status" status)"
grep -q '"repository_count":1' <<<"$first_status" || fail "status did not report the enrolled repository"

printf 'PASS a retried request returned its first result, and a reused key for a different request was refused\n'
