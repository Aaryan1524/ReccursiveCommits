#!/usr/bin/env bash
# Proves a person can name the exact moment their work ships, and that naming one does not weaken
# the policy that decides when this repository is allowed to publish at all.
#
# The two halves matter equally. A requested time that is honoured approximately is worse than one
# refused: somebody who said 10:30 and was quietly given 14:05 has been told a release is
# scheduled and not told when. And a requested time that bypasses the policy would turn the
# schedule from an authorization into a suggestion.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase12.XXXXXX")"
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
  [[ -f "$fixture_root/daemon.log" ]] && sed -n '1,120p' "$fixture_root/daemon.log" >&2
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

json_number() {
  local field="$1"
  sed -E "s/.*\\\"$field\\\":([0-9]+).*/\\1/"
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
printf '# Requested time fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

# Weekday mornings only, in a fixed zone so the assertions below mean the same thing wherever
# this runs.
cat >"$fixture_root/policy.json" <<'JSON'
{
  "timezone": "America/New_York",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday"],
  "windows": [{"start":{"hour":9,"minute":0},"end":{"hour":12,"minute":0}}],
  "daily_releases": {"minimum": 1, "maximum": 3},
  "minimum_spacing_minutes": 60,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/policy.json" >/dev/null

feature_id="feature_00000000-0000-4000-8000-000000001501"
task_one="task_00000000-0000-4000-8000-000000001502"
task_two="task_00000000-0000-4000-8000-000000001503"
cat >"$fixture_root/plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Ship on a named day at a named time",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_one","name":"First change","dependencies":{},"acceptance_checks":[{"id":"ready","description":"ready"}]},
      {"id":"$task_two","name":"Second change","dependencies":{},"acceptance_checks":[{"id":"ready","description":"ready"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'first\n' >"$workspace_path/first.txt"
package_one="$(cli package capture "$feature_id" --revision 1 --task "$task_one" | json_field package_id)"

# --- Ready work is reported, and reports only work that is genuinely ready. ---
ready_text="$(plain ready)"
grep -q "First change" <<<"$ready_text" || \
  fail "captured work is not offered as ready to schedule: $ready_text"
grep -q "Second change" <<<"$ready_text" && \
  fail "work that has not been captured yet was offered as ready"
grep -q "package_" <<<"$ready_text" && \
  fail "the ready list exposed a package identifier to a person"
grep -q "revision" <<<"$ready_text" && \
  fail "the ready list exposed a plan revision to a person"

unit_one="$(cli release create-unit "$feature_id" --revision 1 --task "$task_one" | json_field unit_id)"

# Once grouped, the task is no longer loose work waiting to be scheduled.
grep -q "First change" <<<"$(plain ready)" && \
  fail "work already grouped into a release unit was offered again as ungrouped"

# --- A time outside the window is refused, and says what is allowed. ---
# 2026-09-18 is a Friday; the window is 09:00-12:00.
if refused="$(cli schedule unit "$unit_one" --package-id "$package_one" --revision 1 \
  --at "2026-09-18 22:30" --zone America/New_York 2>&1)"; then
  fail "a release was scheduled outside this repository's publishing hours"
fi
grep -q "outside" <<<"$refused" || \
  fail "the refusal does not say the time is outside the window: $refused"
grep -q "09:00-12:00" <<<"$refused" || \
  fail "the refusal does not say which hours are open: $refused"

# --- A day the repository does not publish on is refused. ---
# 2026-09-19 is a Saturday.
if weekend="$(cli schedule unit "$unit_one" --package-id "$package_one" --revision 1 \
  --at "2026-09-19 10:30" --zone America/New_York 2>&1)"; then
  fail "a release was scheduled on a day this repository does not publish on"
fi
grep -qi "saturday" <<<"$weekend" || \
  fail "the refusal does not name the day it rejected: $weekend"

# --- A time inside the window is honoured exactly. ---
scheduled="$(cli schedule unit "$unit_one" --package-id "$package_one" --revision 1 \
  --at "2026-09-18 10:30" --zone America/New_York)"
selected="$(json_number selected_at_unix_ms <<<"$scheduled")"
# 2026-09-18 10:30 America/New_York is 14:30 UTC, which is 1789741800000.
[[ "$selected" == "1789741800000" ]] || \
  fail "the requested instant was not stored exactly: got $selected"

# Asking again returns the same slot rather than drawing a new one.
again="$(cli schedule unit "$unit_one" --package-id "$package_one" --revision 1 \
  --at "2026-09-18 10:30" --zone America/New_York)"
[[ "$(json_number selected_at_unix_ms <<<"$again")" == "$selected" ]] || \
  fail "repeating an identical scheduling request drew a different time"

# --- The advisory check the wizard uses agrees with the scheduler that decides. ---
#
# Two implementations of the same rules would eventually disagree, and a person told "yes" at the
# prompt and "no" at the review screen would have no way to know which was right.
allowed="$(cli schedule check "$repository_id" --at "2026-09-18 11:30" --zone America/New_York)"
grep -q '"verdict":"allowed"' <<<"$allowed" || \
  fail "a time the scheduler accepts was not reported as allowed: $allowed"

weekend_check="$(cli schedule check "$repository_id" --at "2026-09-19 10:30" --zone America/New_York)"
grep -q '"reason":"day_not_allowed"' <<<"$weekend_check" || \
  fail "a refused weekday did not report a structured reason: $weekend_check"

late_check="$(cli schedule check "$repository_id" --at "2026-09-18 22:30" --zone America/New_York)"
grep -q '"reason":"outside_windows"' <<<"$late_check" || \
  fail "a time outside the window did not report a structured reason: $late_check"

close_check="$(cli schedule check "$repository_id" --at "2026-09-18 10:45" --zone America/New_York)"
grep -q '"reason":"too_close"' <<<"$close_check" || \
  fail "a crowded time did not report a structured reason: $close_check"
# The message used to end on a bare number: "this repository requires 30".
grep -q "minutes between releases" <<<"$close_check" || \
  fail "the spacing refusal still ends without its unit: $close_check"

# --- Spacing is still enforced against what is already scheduled. ---
printf 'second\n' >"$workspace_path/second.txt"
package_two="$(cli package capture "$feature_id" --revision 1 --task "$task_two" | json_field package_id)"
unit_two="$(cli release create-unit "$feature_id" --revision 1 --task "$task_two" | json_field unit_id)"
if crowded="$(cli schedule unit "$unit_two" --package-id "$package_two" --revision 1 \
  --at "2026-09-18 11:00" --zone America/New_York 2>&1)"; then
  fail "a release was scheduled inside this repository's minimum spacing"
fi
grep -qi "within" <<<"$crowded" || \
  fail "the spacing refusal does not explain itself: $crowded"

# Far enough apart is accepted, so the rule is spacing rather than one-per-day.
spaced="$(cli schedule unit "$unit_two" --package-id "$package_two" --revision 1 \
  --at "2026-09-18 11:30" --zone America/New_York)"
[[ "$(json_number selected_at_unix_ms <<<"$spaced")" == "1789745400000" ]] || \
  fail "a correctly spaced request was not honoured exactly"

# --- The human view shows a readable time and no internals. ---
queue_text="$(plain queue status)"
grep -q "10:30" <<<"$queue_text" || \
  fail "the queue does not show the scheduled clock time: $queue_text"
grep -q "1789741800000" <<<"$queue_text" && \
  fail "the queue printed a raw millisecond timestamp to a person"

# --- And the machine view still carries the exact instant. ---
grep -q '"selected_at_unix_ms":1789741800000' <<<"$(cli queue status)" || \
  fail "the machine-readable queue lost the exact instant"

# --- Scheduling without a requested time still works as it always did. ---
cli schedule withdraw "$unit_two" --reason "checking the policy-chosen path" >/dev/null
drawn="$(cli schedule unit "$unit_two" --package-id "$package_two" --revision 1)"
drawn_at="$(json_number selected_at_unix_ms <<<"$drawn")"
[[ -n "$drawn_at" ]] || fail "scheduling without a requested time stopped selecting a slot"

printf 'PASS a requested release time is honoured exactly, refused with a reason when the policy forbids it, and the policy-chosen path is unchanged\n'
