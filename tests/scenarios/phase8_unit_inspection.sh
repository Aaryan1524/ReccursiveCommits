#!/usr/bin/env bash
# Proves a queued unit can be inspected before it publishes: what it changes, and what actually
# ran against it.
#
# The check results matter most. They have been recorded durably since Phase 2 — command, exit
# code, timeout, scrubbed output — and until now no command could read them back. "Did it pass its
# checks" is the first question anyone asks about queued work.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8d.XXXXXX")"
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

cli() { "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json "$@"; }
plain() { "$binary_dir/reccursive" --state-dir "$fixture_root/state" "$@"; }

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Inspection fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli setup "$fixture_root/repository" --non-interactive --timezone UTC | json_field repository_id)"

feature_id="feature_00000000-0000-4000-8000-000000000f01"
task_id="task_00000000-0000-4000-8000-000000000f02"
cat >"$fixture_root/plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Be inspectable before publishing",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Inspectable work","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'first line\nsecond line\n' >"$workspace_path/added.txt"
printf '# Inspection fixture\nand a changed line\n' >"$workspace_path/README.md"
package_id="$(cli task submit "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"

# ---- What the unit changes, before anything is published ----

changes="$(cli package changes "$package_id")"
grep -q '"path":"added.txt"' <<<"$changes" || fail "the added file is not reported: $changes"
grep -q '"path":"README.md"' <<<"$changes" || fail "the modified file is not reported: $changes"
grep -q '"change":"A"' <<<"$changes" || fail "an added file is not reported as added: $changes"
grep -q '"change":"M"' <<<"$changes" || fail "a modified file is not reported as modified: $changes"
grep -q '"patch":null' <<<"$changes" || \
  fail "the patch was included without being asked for, which is what the size limit exists to avoid"

readable="$(plain package changes "$package_id")"
grep -q "2 file(s) changed" <<<"$readable" || fail "the change count is wrong: $readable"

# The patch itself only when asked for, and it contains the work.
patched="$(cli package changes "$package_id" --patch)"
grep -q "second line" <<<"$patched" || fail "the patch does not contain the captured change"
grep -q '"patch_truncated":false' <<<"$patched" || \
  fail "a small patch was reported as truncated: $patched"

# Nothing about inspecting a package may change it: the queue is read, not touched.
before_status="$(cli feature status "$feature_id" --revision 1)"
cli package changes "$package_id" --patch >/dev/null
cli package checks "$package_id" >/dev/null
[[ "$(cli feature status "$feature_id" --revision 1)" == "$before_status" ]] || \
  fail "inspecting a package changed durable state"

# ---- What actually ran against it ----

checks="$(cli package checks "$package_id")"
grep -q '"check_id":"git_diff_check"' <<<"$checks" || \
  fail "the repository's own check is not reported: $checks"
grep -q '"exit_code":0' <<<"$checks" || fail "a passing check is not reported as passing: $checks"
readable_checks="$(plain package checks "$package_id")"
grep -q "At capture, in the package's own workspace" <<<"$readable_checks" || \
  fail "capture-time checks are not labelled: $readable_checks"
grep -q "passed" <<<"$readable_checks" || fail "a passing check is not readable: $readable_checks"

# ---- A failing check is recorded and readable, which is the point of keeping evidence ----

# The repository's default check is `git diff --check`, which fails on a whitespace error. This is
# a real check failing on real content, not a simulated result.
second_task="task_00000000-0000-4000-8000-000000000f03"
cat >"$fixture_root/plan-2.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 2,
  "repository_id": "$repository_id",
  "goal": "Be inspectable before publishing",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$second_task","name":"Work that fails its check","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan-2.json" >/dev/null
second_workspace="$(cli workspace create "$feature_id" --revision 2 | json_field path)"
printf 'trailing whitespace here \n' >"$second_workspace/whitespace.txt"
cli task submit "$feature_id" --revision 2 --task "$second_task" \
  >"$fixture_root/failed-submit.json" 2>&1 || true

# Whether the submission itself was refused or the task was blocked, the evidence must be readable
# either way — that is what a durable check result is for.
failed_package="$(cli package changes 2>/dev/null >/dev/null; cli feature status "$feature_id" --revision 2 | sed -E 's/.*"package_id":"([^"]+)".*/\1/')"
if [[ "$failed_package" == package_* ]]; then
  failed_checks="$(cli package checks "$failed_package")"
  grep -q '"check_id":"git_diff_check"' <<<"$failed_checks" || \
    fail "the failing check left no readable evidence: $failed_checks"
  grep -q '"exit_code":0' <<<"$failed_checks" && \
    fail "a check that failed is reported as passing: $failed_checks"
  readable_failure="$(plain package checks "$failed_package")"
  grep -q "failed" <<<"$readable_failure" || \
    fail "a failed check is not readable as failed: $readable_failure"
else
  grep -qi "check" "$fixture_root/failed-submit.json" || \
    fail "a submission blocked by a check did not say so: $(cat "$fixture_root/failed-submit.json")"
fi

# ---- Inspecting something that does not exist is a clean not-found ----

cli package changes package_00000000-0000-4000-8000-0000000000ff \
  >"$fixture_root/missing.json" 2>&1 && fail "inspecting a missing package was accepted"
grep -q '"code":"not_found"' "$fixture_root/missing.json" || \
  fail "a missing package was not reported as not found: $(cat "$fixture_root/missing.json")"

printf 'PASS a queued unit reports what it changes and what actually ran against it\n'
