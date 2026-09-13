#!/usr/bin/env bash
# Proves an agent can submit one unit of work twice without duplicating it, using no idempotency
# key at all.
#
# The key from P6-T01 protects a *retry of one request*. This is the weaker, more common case: an
# agent that cannot tell whether its earlier submission landed, and simply submits again. Nothing
# here passes --idempotency-key, so what is being checked is that the submit path is repeat-safe on
# its own.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase6b.XXXXXX")"
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
printf '# Agent submit fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

feature_id="feature_00000000-0000-4000-8000-000000000801"
first_task="task_00000000-0000-4000-8000-000000000802"
second_task="task_00000000-0000-4000-8000-000000000803"

# Imported as a draft. An agent that is still deciding scope should be able to register a plan
# before it is willing to fix it.
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Submit one unit twice",
  "target": "refs/heads/main",
  "sealed": false,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$first_task","name":"First unit","dependencies":{},"acceptance_checks":[{"id":"a","description":"present"}]},
      {"id":"$second_task","name":"Second unit","dependencies":{},"acceptance_checks":[{"id":"b","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/feature-plan.json" >/dev/null

# A draft cannot be worked on: the workspace is refused until the scope is fixed.
if cli workspace create "$feature_id" --revision 1 >"$fixture_root/draft.json" 2>&1; then
  fail "an owned workspace was created for a draft plan revision"
fi

# Sealing appends. A stored revision means one thing forever, so the sealed plan is a new revision
# and the response says which — an agent must carry that number forward.
sealed_revision="$(cli plan seal "$feature_id" | sed -E 's/.*"revision":([0-9]+).*/\1/')"
[[ "$sealed_revision" == "2" ]] || fail "sealing did not append revision 2, got $sealed_revision"
cli plan show "$feature_id" --revision 1 | grep -q '"sealed":false' || \
  fail "sealing modified the draft revision instead of appending a new one"
cli plan seal "$feature_id" >"$fixture_root/reseal.json" 2>&1 && fail "an already-sealed plan was sealed again"
grep -q '"code":"conflict"' "$fixture_root/reseal.json" || \
  fail "re-sealing was not refused as a conflict: $(cat "$fixture_root/reseal.json")"

cat >"$fixture_root/schedule-policy.json" <<'JSON'
{
  "timezone": "UTC",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":0,"minute":0},"end":{"hour":23,"minute":59}}],
  "daily_releases": {"minimum": 1, "maximum": 20},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null

workspace_path="$(cli workspace create "$feature_id" --revision "$sealed_revision" | json_field path)"
printf 'first unit\n' >"$workspace_path/first.txt"

# Before anything is submitted, the task reports as planned with nothing attached. An agent needs
# to tell "not started" from "in progress" from "never happening", which is why the status is
# reported verbatim rather than as a ready flag.
status_before="$(cli feature status "$feature_id")"
grep -q '"status":"planned"' <<<"$status_before" || fail "the task did not start as planned"
grep -q '"package_id":null' <<<"$status_before" || fail "an unsubmitted task already had a package"

# The submission itself, and then the same submission again with no key.
first_submission="$(cli task submit "$feature_id" --task "$first_task")"
grep -q '"created":true' <<<"$first_submission" || fail "the first submission did not report creating work"
first_package="$(json_field package_id <<<"$first_submission")"
first_unit="$(json_field unit_id <<<"$first_submission")"
first_due="$(sed -E 's/.*"selected_at_unix_ms":([0-9]+).*/\1/' <<<"$first_submission")"
[[ "$first_package" == package_* && "$first_unit" == unit_* ]] || \
  fail "the submission did not return a package and a unit"

repeat_submission="$(cli task submit "$feature_id" --task "$first_task")"
grep -q '"created":false' <<<"$repeat_submission" || \
  fail "the repeated submission reported creating work a second time"
[[ "$(json_field package_id <<<"$repeat_submission")" == "$first_package" ]] || \
  fail "the repeated submission returned a different package"
[[ "$(json_field unit_id <<<"$repeat_submission")" == "$first_unit" ]] || \
  fail "the repeated submission returned a different release unit"
[[ "$(sed -E 's/.*"selected_at_unix_ms":([0-9]+).*/\1/' <<<"$repeat_submission")" == "$first_due" ]] || \
  fail "the repeated submission drew a new release time"

# Durable state agrees: one package, one unit, one slot — not two of anything.
status_after="$(cli feature status "$feature_id")"
[[ "$(grep -o "$first_package" <<<"$status_after" | wc -l | tr -d ' ')" == "1" ]] || \
  fail "the repeated submission left more than one package attached"
[[ "$(cli release attempts --limit 10 | grep -o '"attempt_id"' | wc -l | tr -d ' ')" == "0" ]] || \
  fail "submitting started a publication attempt; submission only queues work"
grep -q '"status":"scheduled"' <<<"$status_after" || \
  fail "the submitted task is not reported as scheduled"

# A second, genuinely different task is a different submission and is created normally.
printf 'second unit\n' >"$workspace_path/second.txt"

# Task order cannot turn a conflicting submission into a partial capture. The first task already
# belongs to the first package, while the second has never been captured; asking for both is not a
# repeat of either. In particular, naming the new task first proves the service checks the whole
# set before it writes a package.
packages_before_mixed="$(ls "$fixture_root/state/packages" | wc -l | tr -d ' ')"
if cli task submit "$feature_id" --task "$second_task" --task "$first_task" \
  >"$fixture_root/mixed-submission.json" 2>&1; then
  fail "a mixed old/new task submission was accepted"
fi
grep -q '"code":"conflict"' "$fixture_root/mixed-submission.json" || \
  fail "a mixed old/new task submission was not rejected as a conflict: $(cat "$fixture_root/mixed-submission.json")"
packages_after_mixed="$(ls "$fixture_root/state/packages" | wc -l | tr -d ' ')"
[[ "$packages_after_mixed" == "$packages_before_mixed" ]] || \
  fail "a mixed old/new task submission created a package before being refused"

second_submission="$(cli task submit "$feature_id" --task "$second_task")"
grep -q '"created":true' <<<"$second_submission" || fail "a distinct task was not submitted as new work"
[[ "$(json_field unit_id <<<"$second_submission")" != "$first_unit" ]] || \
  fail "a distinct task was grouped into the first unit"

second_due="$(sed -E 's/.*"selected_at_unix_ms":([0-9]+).*/\1/' <<<"$second_submission")"
[[ "$second_due" != "$first_due" ]] || \
  fail "two units were given the same release time; spacing is not being applied"

# Two submissions of the same task at the same moment. The steps each take and release the store
# lock, so without serialization both would find nothing captured and both would capture — leaving
# two packages, one of which no unit publishes.
third_task="task_00000000-0000-4000-8000-000000000804"
cat >"$fixture_root/plan-revision-3.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 3,
  "repository_id": "$repository_id",
  "goal": "Submit one unit twice",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$third_task","name":"Raced unit","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan-revision-3.json" >/dev/null
raced_workspace="$(cli workspace create "$feature_id" --revision 3 | json_field path)"
printf 'raced\n' >"$raced_workspace/raced.txt"
packages_before="$(ls "$fixture_root/state/packages" | wc -l | tr -d ' ')"
cli task submit "$feature_id" --revision 3 --task "$third_task" >"$fixture_root/race-a.json" 2>&1 &
race_a=$!
cli task submit "$feature_id" --revision 3 --task "$third_task" >"$fixture_root/race-b.json" 2>&1 &
race_b=$!
wait "$race_a" || fail "a concurrent submission failed: $(cat "$fixture_root/race-a.json")"
wait "$race_b" || fail "a concurrent submission failed: $(cat "$fixture_root/race-b.json")"
created_count="$(cat "$fixture_root/race-a.json" "$fixture_root/race-b.json" | grep -o '"created":true' | wc -l | tr -d ' ')"
[[ "$created_count" == "1" ]] || \
  fail "$created_count of two concurrent submissions reported creating work; exactly one should"
packages_after="$(ls "$fixture_root/state/packages" | wc -l | tr -d ' ')"
[[ "$((packages_after - packages_before))" == "1" ]] || \
  fail "two concurrent submissions captured $((packages_after - packages_before)) packages instead of one"

printf 'PASS one unit submitted twice — sequentially and concurrently — produced one package, one unit, and one release time\n'
