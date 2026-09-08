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

printf 'PASS Phase 1 CLI enrollment, conflict handling, and restart persistence\n'
