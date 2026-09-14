#!/usr/bin/env bash
# Proves the queue can be read one-shot or watched live, and that stopping a watch stops only the
# display — the daemon keeps running and queued work keeps its place.
#
# Watching is the part with a real hazard: a live view that held a lease, claimed work, or shared a
# lifetime with the service would make "press Ctrl-C" a destructive act. It does not, and that is
# what the last section here measures rather than asserts.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8c.XXXXXX")"
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
printf '# Queue fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

# A window far enough out that nothing publishes underneath this test: the queue must stay legible
# while it is being read.
cat >"$fixture_root/policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":23,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 5},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/policy.json" >/dev/null

feature_id="feature_00000000-0000-4000-8000-000000000d01"
task_id="task_00000000-0000-4000-8000-000000000d02"
cat >"$fixture_root/plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Be visible in the queue",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Queued work","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan.json" >/dev/null

# ---- An empty queue says so rather than showing nothing ----

empty_queue="$(plain queue status)"
grep -q "nothing queued" <<<"$empty_queue" || \
  fail "an enrolled repository with no work did not report an empty queue: $empty_queue"

# ---- Submitted work appears, named by its plan task ----

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'queued\n' >"$workspace_path/queued.txt"
submission="$(cli task submit "$feature_id" --revision 1 --task "$task_id")"
unit_id="$(json_field unit_id <<<"$submission")"

queued="$(cli queue status)"
grep -q "$unit_id" <<<"$queued" || fail "the submitted unit is not in the queue: $queued"
grep -q '"state":"scheduled"' <<<"$queued" || \
  fail "a unit with a future release time is not reported as scheduled: $queued"
readable="$(plain queue status)"
grep -q "Queued work" <<<"$readable" || \
  fail "the queue does not name the work by its plan task: $readable"

# Restricting to one repository returns that repository only.
scoped="$(cli queue status --repository-id "$repository_id")"
grep -q "$repository_id" <<<"$scoped" || fail "the scoped queue omitted its own repository"

# ---- Withdrawing a release time is explained, not silent ----

cli schedule withdraw "$unit_id" --reason "policy changed during review" >/dev/null
withdrawn="$(cli queue status)"
grep -q '"state":"ready"' <<<"$withdrawn" || \
  fail "a unit whose time was withdrawn is not reported as ready again: $withdrawn"
grep -q "policy changed during review" <<<"$withdrawn" || \
  fail "the queue does not say why the release time was withdrawn: $withdrawn"

history="$(cli schedule history "$unit_id")"
grep -q "policy changed during review" <<<"$history" || \
  fail "schedule history does not record why the time was withdrawn: $history"
readable_history="$(plain schedule history "$unit_id")"
grep -q "SELECTED" <<<"$readable_history" || fail "schedule history is not readable: $readable_history"

# ---- A paused repository says so ----

cli schedule pause "$repository_id" --reason "holding releases for a review" >/dev/null
paused="$(plain queue status)"
grep -q "holding releases for a review" <<<"$paused" || \
  fail "a paused repository does not report why: $paused"
cli schedule resume "$repository_id" >/dev/null

# ---- Watching is a display, and stopping it stops only the display ----

# Re-select a time so there is durable state to compare across the watch.
cli schedule unit "$unit_id" --package-id "$(json_field package_id <<<"$submission")" --revision 1 >/dev/null
# The summary carries the moment it was generated, which differs on every call by design. What
# must not change is the queued work itself, so the timestamp is excluded from the comparison.
queue_without_timestamp() {
  sed -E 's/"generated_at_unix_ms":[0-9]+,?//' <<<"$(cli queue status)"
}
before_watch="$(queue_without_timestamp)"
before_slot="$(cli schedule show "$unit_id")"

plain queue watch --interval 1 >"$fixture_root/watch.log" 2>&1 &
watch_pid=$!
sleep 3
kill -0 "$watch_pid" 2>/dev/null || fail "the watch exited on its own before it was stopped"
# Redrawing at all is what makes it a watch rather than a one-shot.
[[ "$(grep -c "$unit_id" "$fixture_root/watch.log")" -ge 2 ]] || \
  fail "the watch did not redraw: $(cat "$fixture_root/watch.log")"

# Stop it the way a person would.
kill -TERM "$watch_pid" 2>/dev/null || true
wait "$watch_pid" 2>/dev/null || true
kill -0 "$watch_pid" 2>/dev/null && fail "the watch did not stop when it was terminated"

# This is the exit criterion. The service is untouched and so is the queued work.
kill -0 "$daemon_pid" 2>/dev/null || fail "stopping the watch stopped the daemon"
cli status >/dev/null || fail "the daemon stopped answering after the watch was stopped"
[[ "$(queue_without_timestamp)" == "$before_watch" ]] || \
  fail "stopping the watch changed the queue"
[[ "$(cli schedule show "$unit_id")" == "$before_slot" ]] || \
  fail "stopping the watch changed the unit's selected release time"

# ---- A bounded watch ends on its own, which is what lets a script use it ----

started="$(date +%s)"
plain queue watch --interval 1 --for 3 >"$fixture_root/bounded.log" 2>&1
elapsed=$(( $(date +%s) - started ))
[[ "$elapsed" -le 10 ]] || fail "a bounded watch ran for ${elapsed}s instead of stopping"
grep -q "$unit_id" "$fixture_root/bounded.log" || fail "a bounded watch drew nothing"

printf 'PASS the queue reads one-shot and live, explains withdrawn times, and stopping the watch left the daemon and the queue untouched\n'
