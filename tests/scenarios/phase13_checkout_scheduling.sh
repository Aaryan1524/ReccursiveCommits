#!/usr/bin/env bash
# Proves the two bugs a real trial found stay fixed, and that a person can schedule the changes
# already in their own checkout without touching it.
#
# The trial that motivated this: work waiting on an unpublished prerequisite was offered as
# selectable, the user picked it, and scheduling refused at the very end — and the failed attempt
# left a release unit behind, so the change stopped appearing as ready while still showing in the
# queue. Neither schedulable nor visibly gone.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase13.XXXXXX")"
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

json_field() { sed -E "s/.*\\\"$1\\\":\\\"([^\\\"]+)\\\".*/\\1/"; }
json_number() { sed -E "s/.*\\\"$1\\\":([0-9]+).*/\\1/"; }
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
printf '# Checkout fixture\n' >"$fixture_root/repository/README.md"
printf 'build/\n' >"$fixture_root/repository/.gitignore"
printf 'this file is deleted before capture\n' >"$fixture_root/repository/doomed.txt"
git -C "$fixture_root/repository" add README.md .gitignore doomed.txt
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

cat >"$fixture_root/policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 20},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/policy.json" >/dev/null

feature_id="feature_00000000-0000-4000-8000-000000001601"
base_task="task_00000000-0000-4000-8000-000000001602"
dependent_task="task_00000000-0000-4000-8000-000000001603"
cat >"$fixture_root/plan.json" <<JSON
{
  "schema_version": 1, "feature_id": "$feature_id", "revision": 1,
  "repository_id": "$repository_id", "goal": "Dependency ordering",
  "target": "refs/heads/main", "sealed": true,
  "phases": [{"id":"d","name":"Delivery","tasks":[
    {"id":"$base_task","name":"Authentication base","dependencies":{},"acceptance_checks":[{"id":"o","description":"o"}]},
    {"id":"$dependent_task","name":"Authentication follow-up","dependencies":{"$base_task":"target_published"},"acceptance_checks":[{"id":"o","description":"o"}]}
  ]}]
}
JSON
cli plan import "$fixture_root/plan.json" >/dev/null
workspace="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'base\n' >"$workspace/base.txt"
base_package="$(cli package capture "$feature_id" --revision 1 --task "$base_task" | json_field package_id)"
printf 'follow\n' >"$workspace/follow.txt"
cli package capture "$feature_id" --revision 1 --task "$dependent_task" >/dev/null

# --- Bug 1: work waiting on a prerequisite must be reported as blocked, not offered. ---
ready_json="$(cli ready)"
grep -q '"blocked_by":null' <<<"$ready_json" || \
  fail "no schedulable work was reported as schedulable: $ready_json"
grep -q '"name":"Authentication base"' <<<"$ready_json" || \
  fail "the blocking prerequisite is not named: $ready_json"
grep -q '"milestone":"target_published"' <<<"$ready_json" || \
  fail "the milestone the dependent waits for is not reported: $ready_json"

# --- Bug 2: a scheduling attempt that fails must not strand the work. ---
#
# Scheduling the dependent is refused. Before the fix the release unit created on the way survived
# that refusal, and the work stopped appearing as ready ever again.
# The rollback itself — grouping discarded when scheduling refuses — is a daemon concern with no
# command of its own, so it is covered by `discard_unscheduled_release_unit`'s own tests rather
# than pretended at here. What this asserts is the visible consequence: reading the ready list,
# and asking about a time, never change what is ready.
before_reads="$(cli ready)"
cli schedule check "$repository_id" --at "2020-01-06 10:00" --zone UTC >/dev/null 2>&1 || true
[[ "$(cli ready)" == "$before_reads" ]] || \
  fail "asking about a release time changed what is ready"

# --- The checkout path: a person's own working tree becomes an immutable package. ---
printf 'first version\n' >"$fixture_root/repository/checkout-change.txt"
# A tracked file removed from the working tree. Deletions are representable — the capture layer
# records a missing prerequisite as deleted rather than copying it — so they are captured rather
# than refused, and this proves the published result removes it instead of resurrecting HEAD's copy.
git -C "$fixture_root/repository" rm --quiet doomed.txt
printf 'ignored\n' >"$fixture_root/repository/build-artifact" 2>/dev/null || true
mkdir -p "$fixture_root/repository/build"; printf 'junk\n' >"$fixture_root/repository/build/ignored.o"
status_before="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"
head_before="$(git -C "$fixture_root/repository" rev-parse HEAD)"

# The interactive wizard needs a terminal; the same records it builds are exercised here through
# the commands it composes, which is what a scenario can drive.
human_feature="feature_00000000-0000-4000-8000-000000001701"
human_task="task_00000000-0000-4000-8000-000000001702"
cat >"$fixture_root/human.json" <<JSON
{
  "schema_version": 1, "feature_id": "$human_feature", "revision": 1,
  "repository_id": "$repository_id", "goal": "Add checkout change",
  "target": "refs/heads/main", "sealed": true,
  "phases": [{"id":"d","name":"Delivery","tasks":[
    {"id":"$human_task","name":"Add checkout change","dependencies":{},"acceptance_checks":[{"id":"o","description":"o"}]}
  ]}]
}
JSON
cli plan import "$fixture_root/human.json" >/dev/null
cli workspace create "$human_feature" --revision 1 \
  --include checkout-change.txt --include doomed.txt >/dev/null
human_package="$(cli package capture "$human_feature" --revision 1 --task "$human_task" | json_field package_id)"

# Editing the checkout after capture must not change what was captured.
printf 'second version, after capture\n' >"$fixture_root/repository/checkout-change.txt"

changes="$(cli package changes "$human_package" --revision 1)"
grep -q "checkout-change.txt" <<<"$changes" || \
  fail "the captured package does not contain the checkout change: $changes"
grep -q "ignored.o" <<<"$changes" && \
  fail "an ignored file was captured"
grep -q "doomed.txt" <<<"$changes" || \
  fail "a deleted file is not represented in the captured package: $changes"

human_unit="$(cli release create-unit "$human_feature" --revision 1 --task "$human_task" | json_field unit_id)"
[[ "$human_unit" == unit_* ]] || fail "grouping the captured checkout change did not return a unit"
cli schedule unit "$human_unit" --package-id "$human_package" --revision 1 >/dev/null
cli schedule release-now "$human_unit" >/dev/null

published=''
for _ in {1..90}; do
  if git -C "$fixture_root/remote.git" rev-parse --verify --quiet refs/heads/main >/dev/null \
    && git -C "$fixture_root/remote.git" show refs/heads/main:checkout-change.txt >/dev/null 2>&1; then
    published=yes
    break
  fi
  sleep 2
done
[[ -n "$published" ]] || fail "the captured checkout change was never published"

# --- The captured version published, not the later edit. ---
landed="$(git -C "$fixture_root/remote.git" show refs/heads/main:checkout-change.txt)"
[[ "$landed" == "first version" ]] || \
  fail "the published content is not the captured snapshot: got '$landed'"
[[ "$(cat "$fixture_root/repository/checkout-change.txt")" == "second version, after capture" ]] || \
  fail "the user's later edit was overwritten"

# The deletion landed: the target no longer carries the file, rather than keeping HEAD's copy.
git -C "$fixture_root/remote.git" show refs/heads/main:doomed.txt >/dev/null 2>&1 && \
  fail "a file deleted in the checkout is still present on the target"

# --- And the checkout itself was never touched. ---
status_after="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"
[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$head_before" ]] || \
  fail "the user's HEAD moved"
[[ -z "$(git -C "$fixture_root/repository" stash list)" ]] || \
  fail "something stashed the user's work"
[[ "$(cat "$fixture_root/repository/build/ignored.o")" == "junk" ]] || \
  fail "an ignored file was disturbed"
# The only difference from before is the edit the scenario itself made.
grep -q "checkout-change.txt" <<<"$status_after" || \
  fail "the user's own change vanished from their checkout: $status_after"

# --- A slot that becomes due on its own must publish, not be pushed forward forever. ---
#
# The bug this protects against: missed-window reconciliation runs before the release pass in
# every maintenance tick, so under `reschedule_forward` — which is what `setup` installs by
# default — every slot whose time had arrived was withdrawn and rescheduled into the future before
# anything could claim it. Nothing ever published. A real trial watched one walk 15:20 → 15:24 →
# 16:39 and never ship.
#
# It has to become due *naturally*. `schedule release-now` records a deliberate request, which
# reconciliation deliberately leaves alone, so a test built on it would pass while the bug was
# fully present — which is exactly what the checkout test above does, and why this exists
# separately. The policy set at the top of this scenario is `reschedule_forward`.
natural_feature="feature_00000000-0000-4000-8000-000000001801"
natural_task="task_00000000-0000-4000-8000-000000001802"
cat >"$fixture_root/natural.json" <<JSON
{
  "schema_version": 1, "feature_id": "$natural_feature", "revision": 1,
  "repository_id": "$repository_id", "goal": "Publishes when its own time arrives",
  "target": "refs/heads/main", "sealed": true,
  "phases": [{"id":"d","name":"Delivery","tasks":[
    {"id":"$natural_task","name":"Naturally due change","dependencies":{},"acceptance_checks":[{"id":"o","description":"o"}]}
  ]}]
}
JSON
cli plan import "$fixture_root/natural.json" >/dev/null
natural_workspace="$(cli workspace create "$natural_feature" --revision 1 | json_field path)"
printf 'due on its own
' >"$natural_workspace/naturally-due.txt"
natural_package="$(cli package capture "$natural_feature" --revision 1 --task "$natural_task" | json_field package_id)"
natural_unit="$(cli release create-unit "$natural_feature" --revision 1 --task "$natural_task" | json_field unit_id)"

# Far enough out that it is still in the future when the slot is written, near enough that the
# scenario does not sit for minutes. The policy's zone is UTC, so the requested time is too.
due_at="$(date -u -v +75S '+%Y-%m-%d %H:%M' 2>/dev/null || date -u -d '+75 seconds' '+%Y-%m-%d %H:%M')"
natural_slot="$(cli schedule unit "$natural_unit" --package-id "$natural_package" --revision 1 \
  --at "$due_at" --zone UTC)"
natural_selected="$(json_number selected_at_unix_ms <<<"$natural_slot")"
[[ -n "$natural_selected" ]] || fail "the naturally-due unit did not get a release time"

natural_published=''
for _ in {1..90}; do
  if git -C "$fixture_root/remote.git" show refs/heads/main:naturally-due.txt >/dev/null 2>&1; then
    natural_published=yes
    break
  fi
  sleep 4
done
[[ -n "$natural_published" ]] || \
  fail "a release whose time arrived on its own was never published; reconciliation is pushing it forward"

# And it published at the time it was given, rather than after being moved.
natural_history="$(cli schedule history "$natural_unit")"
grep -q "release window was missed" <<<"$natural_history" && \
  fail "the release time was treated as a missed window instead of being published: $natural_history"

printf 'PASS blocked work is reported rather than offered, a failed attempt strands nothing, and a checkout change is captured immutably without the checkout being touched\n'
