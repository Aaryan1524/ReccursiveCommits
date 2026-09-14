#!/usr/bin/env bash
# Proves immediate-availability mode is real end to end: work reaches a development branch at its
# release time with the target left alone, the integration into the target is planned without
# anyone asking and then lands, and dependencies waiting on either milestone are held or released
# accordingly.
#
# This exists because every part of immediate mode except the acting on it was already shipped —
# the flag was accepted, validated, stored and displayed while the release worker never read it,
# and `development_available` was hardcoded to never be satisfied. Both failures were silent.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase10.XXXXXX")"
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

remote_ref() {
  git -C "$fixture_root/remote.git" rev-parse --verify --quiet "$1" || true
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Development publication fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
main_before="$(remote_ref refs/heads/main)"

# The development branch does not exist yet. Creating it is part of what is being tested: a mode
# that required the branch to be made by hand first would not be the mode that was described.
[[ -z "$(remote_ref refs/heads/development)" ]] || fail "the fixture already has a development branch"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" \
  --mode immediate --development-target development | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "enrollment in immediate mode did not return a repository ID"

feature_id="feature_00000000-0000-4000-8000-000000001001"
foundation="task_00000000-0000-4000-8000-000000001002"
dependent_dev="task_00000000-0000-4000-8000-000000001003"
dependent_target="task_00000000-0000-4000-8000-000000001004"
follow_up="task_00000000-0000-4000-8000-000000001005"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Publish early to a development branch",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$foundation","name":"Add the shared foundation","dependencies":{},"acceptance_checks":[{"id":"present","description":"the foundation is available"}]},
      {"id":"$dependent_dev","name":"Build on the foundation","dependencies":{"$foundation":"development_available"},"acceptance_checks":[{"id":"present","description":"the dependent builds"}]},
      {"id":"$dependent_target","name":"Ship once the target has it","dependencies":{"$foundation":"target_published"},"acceptance_checks":[{"id":"present","description":"the target carries the foundation"}]},
      {"id":"$follow_up","name":"Add a follow-up change","dependencies":{},"acceptance_checks":[{"id":"present","description":"the follow-up is available"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

cat >"$fixture_root/schedule-policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 20},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "catch_up", "max_releases": 5}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null

# One owned workspace per feature revision, so each capture adds its own file and takes only what
# has changed since the last one.
workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace"

capture_unit() {
  local task_id="$1" filename="$2" contents="$3"
  printf '%s\n' "$contents" >"$workspace_path/$filename"
  cli package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id
}

foundation_package="$(capture_unit "$foundation" foundation.txt "the shared foundation")"
[[ "$foundation_package" == package_* ]] || fail "capturing the foundation did not return a package"
foundation_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$foundation" | json_field unit_id)"
[[ "$foundation_unit" == unit_* ]] || fail "the foundation did not produce a release unit"

dependent_dev_package="$(capture_unit "$dependent_dev" dependent.txt "built on the foundation")"
dependent_dev_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$dependent_dev" | json_field unit_id)"
dependent_target_package="$(capture_unit "$dependent_target" shipped.txt "ships after the target has it")"
dependent_target_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$dependent_target" | json_field unit_id)"

# --- Before anything is published, both dependents wait. ---
for pair in "$dependent_dev_unit:$dependent_dev_package" "$dependent_target_unit:$dependent_target_package"; do
  unit="${pair%%:*}"
  package="${pair##*:}"
  if refusal="$(cli schedule unit "$unit" --package-id "$package" --revision 1 2>&1)"; then
    fail "a dependent whose prerequisite is unpublished must not be schedulable"
  fi
  grep -q "waits for prerequisite" <<<"$refusal" || \
    fail "the refusal did not name the unmet prerequisite: $refusal"
done

# --- The foundation publishes to the development branch, on the daemon's own pass. ---
cli schedule unit "$foundation_unit" --package-id "$foundation_package" --revision 1 >/dev/null
cli schedule release-now "$foundation_unit" >/dev/null

development_head=''
for _ in {1..90}; do
  development_head="$(remote_ref refs/heads/development)"
  [[ -n "$development_head" ]] && break
  sleep 2
done
[[ -n "$development_head" ]] || \
  fail "the daemon did not create the development branch within 180 seconds"

# The claim immediate mode actually makes: available early, target untouched.
[[ "$(remote_ref refs/heads/main)" == "$main_before" ]] || \
  fail "publishing in immediate mode moved the target branch"
git -C "$fixture_root/remote.git" merge-base --is-ancestor "$main_before" "$development_head" || \
  fail "the development branch was not built on the target's history"

git clone --quiet --branch development "$fixture_root/remote.git" "$fixture_root/verification"
[[ -f "$fixture_root/verification/foundation.txt" ]] || \
  fail "the development branch does not contain the captured change"
subject="$(git -C "$fixture_root/verification" log -1 --format=%s)"
[[ "$subject" == "Add the shared foundation" ]] || \
  fail "the development commit was not derived from the plan task: $subject"

# --- The selection that published is spent, and recorded as spent. ---
history="$(cli schedule history "$foundation_unit")"
grep -q "published to the development branch" <<<"$history" || \
  fail "the selection that published was not withdrawn with its reason: $history"

# --- The integration is planned on its own: nobody schedules the second half by hand. ---
integration_slot=''
for _ in {1..60}; do
  if integration_slot="$(cli schedule show "$foundation_unit" 2>/dev/null)"; then
    break
  fi
  integration_slot=''
  sleep 2
done
[[ -n "$integration_slot" ]] || \
  fail "no release time was selected for integrating the development work into the target"
grep -q '"type":"schedule_slot"' <<<"$integration_slot" || \
  fail "the integration selection did not return a slot: $integration_slot"

# Withdraw it for now. Its time is drawn at random from the policy window, so leaving it live
# would let the target move at an unpredictable moment and make every later assertion here a
# coin toss. It is scheduled again deliberately at the end.
cli schedule withdraw "$foundation_unit" --reason "scenario integrates deliberately" >/dev/null

# With the work on a development branch and no integration selected, the queue says so in its own
# right rather than reporting the unit as though nothing had ever been published.
withdrawn_queue="$(cli queue status)"
grep -q '"state":"available_early"' <<<"$withdrawn_queue" || \
  fail "a unit on a development branch with no integration scheduled is not reported as available early: $withdrawn_queue"
withdrawn_text="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" queue status)"
grep -q "available early" <<<"$withdrawn_text" || \
  fail "the human-readable queue does not report early availability: $withdrawn_text"

# And prove it rather than only inferring it: sit through a further maintenance pass.
sleep 70
[[ "$(remote_ref refs/heads/development)" == "$development_head" ]] || \
  fail "the development branch moved again after the unit was already published"

# --- The product has to *say* the work is available, not merely have published it. ---
#
# Withdrawing the spent slot returns the unit's tasks to the queue, so without this the unit would
# render exactly like one that had never published at all.
queue_json="$(cli queue status)"
grep -q "\"target\":\"refs/heads/development\"" <<<"$queue_json" || \
  fail "the queue does not report which branch the work reached: $queue_json"
grep -q "\"commit\":\"$development_head\"" <<<"$queue_json" || \
  fail "the queue does not report the commit that carries the work: $queue_json"
grep -q '"is_target":false' <<<"$queue_json" || \
  fail "the queue does not distinguish a development branch from the target: $queue_json"

queue_text="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" queue status)"
grep -q "is available early on refs/heads/development" <<<"$queue_text" || \
  fail "the human-readable queue does not name the development branch: $queue_text"

status_text="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" feature status "$feature_id" --revision 1)"
grep -q "available: refs/heads/development" <<<"$status_text" || \
  fail "feature status does not report where the work is available: $status_text"
grep -q "on target:" <<<"$status_text" && \
  fail "feature status claims work is on the target when only the development branch has it"

# --- The two milestones now diverge, which is the whole reason they are separate. ---
slot_json="$(cli schedule unit "$dependent_dev_unit" --package-id "$dependent_dev_package" --revision 1)"
grep -q '"type":"schedule_slot"' <<<"$slot_json" || \
  fail "a dependent waiting for the development branch was not released by a development publication"
# Withdraw it again so it cannot publish while the rest of the scenario runs.
cli schedule withdraw "$dependent_dev_unit" --reason "scenario keeps the queue still" >/dev/null

if still_waiting="$(cli schedule unit "$dependent_target_unit" --package-id "$dependent_target_package" --revision 1 2>&1)"; then
  fail "a dependent waiting for the target was released by a development publication"
fi
grep -q "waits for prerequisite" <<<"$still_waiting" || \
  fail "the target-milestone refusal did not name the unmet prerequisite: $still_waiting"

# --- A second package stacks on the development branch rather than restarting from the target. ---
follow_up_package="$(capture_unit "$follow_up" follow-up.txt "a later change")"
follow_up_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$follow_up" | json_field unit_id)"
cli schedule unit "$follow_up_unit" --package-id "$follow_up_package" --revision 1 >/dev/null
cli schedule release-now "$follow_up_unit" >/dev/null

follow_up_head=''
for _ in {1..90}; do
  candidate="$(remote_ref refs/heads/development)"
  if [[ -n "$candidate" && "$candidate" != "$development_head" ]]; then
    follow_up_head="$candidate"
    break
  fi
  sleep 2
done
[[ -n "$follow_up_head" ]] || \
  fail "the second immediate-mode unit did not reach the development branch within 180 seconds"
parent="$(git -C "$fixture_root/remote.git" rev-parse "$follow_up_head^")"
[[ "$parent" == "$development_head" ]] || \
  fail "the second unit did not build on the development branch: parent $parent, expected $development_head"

# --- Integration: the same work reaches the target, and only then is the task published. ---
integration_package="$foundation_package"
cli schedule unit "$foundation_unit" --package-id "$integration_package" --revision 1 >/dev/null
cli schedule release-now "$foundation_unit" >/dev/null

integrated=''
for _ in {1..90}; do
  if [[ "$(remote_ref refs/heads/main)" != "$main_before" ]]; then
    integrated=yes
    break
  fi
  sleep 2
done
[[ -n "$integrated" ]] || \
  fail "the development work was never integrated into the target within 180 seconds"

main_after="$(remote_ref refs/heads/main)"
git -C "$fixture_root/remote.git" merge-base --is-ancestor "$main_before" "$main_after" || \
  fail "the integration did not build on the target's existing history"
git clone --quiet --branch main "$fixture_root/remote.git" "$fixture_root/integrated"
[[ -f "$fixture_root/integrated/foundation.txt" ]] || \
  fail "the target does not contain the integrated change"

# The task is published only now — reaching a development branch was never enough.
status_after="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" feature status "$feature_id" --revision 1)"
grep -q "on target: refs/heads/main" <<<"$status_after" || \
  fail "feature status does not report the work as being on the target: $status_after"

# And the dependent that was waiting for the target is finally released.
released="$(cli schedule unit "$dependent_target_unit" --package-id "$dependent_target_package" --revision 1)"
grep -q '"type":"schedule_slot"' <<<"$released" || \
  fail "a dependent waiting for the target was not released by the integration: $released"

printf 'PASS immediate mode publishes early to a development branch, plans and performs its own integration into the target, and settles both dependency milestones correctly\n'
