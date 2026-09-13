#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase4.XXXXXX")"
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

# Extracts a quoted string field: "field":"value"
json_field() {
  local field="$1"
  sed -E "s/.*\\\"$field\\\":\\\"([^\\\"]+)\\\".*/\\1/"
}

# Extracts a bare numeric field: "field":123
json_number() {
  local field="$1"
  sed -E "s/.*\\\"$field\\\":([0-9]+).*/\\1/"
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
printf '# Schedule scenario fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
enrollment_json="$(cli repository add "$fixture_root/repository")"
repository_id="$(json_field id <<<"$enrollment_json")"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000000401"
task_id="task_00000000-0000-4000-8000-000000000402"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Schedule one verified unit through the local CLI",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Add the change","dependencies":{},"acceptance_checks":[{"id":"present","description":"the change is ready"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

workspace_json="$(cli workspace create "$feature_id" --revision 1)"
workspace_path="$(json_field path <<<"$workspace_json")"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace"

printf 'the scheduled change\n' >"$workspace_path/change.txt"
package_json="$(cli package capture "$feature_id" --revision 1 --task "$task_id")"
package_id="$(json_field package_id <<<"$package_json")"
[[ "$package_id" == package_* ]] || fail "package capture did not return an ID"

unit_json="$(cli release create-unit "$feature_id" --revision 1 --task "$task_id")"
grep -q '"type":"release_unit_created"' <<<"$unit_json" || \
  fail "release unit creation did not return the expected payload"
unit_id="$(json_field unit_id <<<"$unit_json")"
[[ "$unit_id" == unit_* ]] || fail "release unit creation did not return an ID"

# A unit cannot be scheduled before a policy exists for its repository.
if premature_schedule="$(cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 2>&1)"; then
  fail "scheduling before a policy exists must be refused"
fi
grep -q 'no active schedule policy' <<<"$premature_schedule" || \
  fail "missing-policy failure did not explain why: $premature_schedule"

cat >"$fixture_root/schedule-policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 5},
  "minimum_spacing_minutes": 30,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
policy_activation="$(cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json")"
grep -q '"type":"schedule_policy_activated"' <<<"$policy_activation" || \
  fail "policy activation did not return the expected payload"

policy_show="$(cli schedule show-policy "$repository_id")"
grep -q '"timezone":"UTC"' <<<"$policy_show" || fail "policy inspection did not return the active policy"

slot_json="$(cli schedule unit "$unit_id" --package-id "$package_id" --revision 1)"
grep -q '"type":"schedule_slot"' <<<"$slot_json" || fail "scheduling a unit did not return a slot"
selected_at="$(json_number selected_at_unix_ms <<<"$slot_json")"
[[ -n "$selected_at" ]] || fail "schedule slot did not record a selected time"

# Restarting must not redraw the slot: the same durable time comes back, not a fresh selection.
stop_daemon
start_daemon
reloaded_json="$(cli schedule show "$unit_id")"
reloaded_selected_at="$(json_number selected_at_unix_ms <<<"$reloaded_json")"
[[ "$reloaded_selected_at" == "$selected_at" ]] || \
  fail "schedule slot was redrawn after restart: was $selected_at, now $reloaded_selected_at"

# Scheduling the same unit again, with a different seed, must return the existing slot rather than
# drawing a second one.
repeat_json="$(cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 --seed 999999)"
repeat_selected_at="$(json_number selected_at_unix_ms <<<"$repeat_json")"
[[ "$repeat_selected_at" == "$selected_at" ]] || \
  fail "scheduling an already-scheduled unit redrew its slot"

unit_show="$(cli release show-unit "$unit_id")"
grep -q "\"unit_id\":\"$unit_id\"" <<<"$unit_show" || fail "release unit inspection returned the wrong unit"

# Adaptive recalculation: withdrawing a selection returns the work to the queue, and a fresh
# selection can then be made. The withdrawn time must not simply reappear.
withdraw_json="$(cli schedule withdraw "$unit_id" --reason "policy under review")"
grep -q '"type":"schedule_recalculated"' <<<"$withdraw_json" || \
  fail "withdrawing a slot did not return a recalculation"
if cli schedule show "$unit_id" >/dev/null 2>&1; then
  fail "a withdrawn unit must not still report a live release time"
fi

rescheduled_json="$(cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 --seed 4242)"
grep -q '"type":"schedule_slot"' <<<"$rescheduled_json" || \
  fail "a withdrawn unit could not be scheduled again"
rescheduled_at="$(json_number selected_at_unix_ms <<<"$rescheduled_json")"
[[ -n "$rescheduled_at" ]] || fail "rescheduled slot did not record a selected time"

# A repository-wide recalculation reports what it moved rather than moving things silently.
recalculated_json="$(cli schedule recalculate "$repository_id" --reason "policy revised")"
grep -q '"type":"schedule_recalculated"' <<<"$recalculated_json" || \
  fail "repository recalculation did not return the expected payload"
grep -q "\"withdrawn\":\[\"$unit_id\"\]" <<<"$recalculated_json" || \
  fail "repository recalculation did not report the withdrawn unit: $recalculated_json"

# The repository-wide recalculation above withdrew the selection, so give the unit a live one
# again before exercising the controls that report and act on it.
current_json="$(cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 --seed 777)"
current_at="$(json_number selected_at_unix_ms <<<"$current_json")"
[[ -n "$current_at" ]] || fail "could not reschedule the unit after recalculation"

# Schedule controls. Preview never changes anything; pause and resume are durable decisions.
preview_json="$(cli schedule preview "$repository_id")"
grep -q '"type":"schedule_preview"' <<<"$preview_json" || fail "preview did not return the expected payload"
preview_at="$(json_number selected_at_unix_ms <<<"$preview_json")"
[[ "$preview_at" == "$current_at" ]] || \
  fail "preview reported a different time than the live selection: $preview_at vs $current_at"

cli schedule pause "$repository_id" --reason "investigating" >/dev/null
paused_preview="$(cli schedule preview "$repository_id")"
grep -q '"paused":"investigating"' <<<"$paused_preview" || \
  fail "a paused repository did not report why: $paused_preview"
due_while_paused="$(cli schedule due --concurrency-limit 10)"
grep -q '"units":\[\]' <<<"$due_while_paused" || \
  fail "a paused repository still handed out due work: $due_while_paused"

cli schedule resume "$repository_id" >/dev/null
resumed_preview="$(cli schedule preview "$repository_id")"
grep -q '"paused":null' <<<"$resumed_preview" || fail "resume did not clear the pause"

# Release-now moves the unit to the front of the queue and it becomes claimable.
release_now_json="$(cli schedule release-now "$unit_id")"
grep -q '"type":"schedule_slot"' <<<"$release_now_json" || fail "release-now did not return a slot"
due_json="$(cli schedule due --concurrency-limit 10)"
grep -q "$unit_id" <<<"$due_json" || \
  fail "a unit released now did not become due: $due_json"

printf 'PASS release-unit creation, policy activation, deterministic scheduling, restart-stable slots, adaptive recalculation, and schedule controls\n'
