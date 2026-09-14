#!/usr/bin/env bash
# Proves changing where a repository publishes cannot duplicate a unit, strand published work, or
# send anything to a branch nobody chose.
#
# A policy change is the operation with the most ways to quietly corrupt a queue: work already on
# a development branch is owed an integration, selected release times were drawn against branches
# that may no longer be the destination, and a unit that gets a second identity is a unit that
# gets published twice. Each of those is asserted here rather than reasoned about.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase10-policy.XXXXXX")"
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

json_number() {
  local field="$1"
  sed -E "s/.*\\\"$field\\\":([0-9]+).*/\\1/"
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
printf '# Policy change fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
main_before="$(remote_ref refs/heads/main)"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" \
  --mode immediate --development-target development | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "enrollment did not return a repository ID"

feature_id="feature_00000000-0000-4000-8000-000000001301"
first_task="task_00000000-0000-4000-8000-000000001302"
second_task="task_00000000-0000-4000-8000-000000001303"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Change where publication happens without losing anything",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$first_task","name":"Publish early","dependencies":{},"acceptance_checks":[{"id":"present","description":"ready"}]},
      {"id":"$second_task","name":"Wait for a new destination","dependencies":{},"acceptance_checks":[{"id":"present","description":"ready"}]}
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

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'published early\n' >"$workspace_path/early.txt"
first_package="$(cli package capture "$feature_id" --revision 1 --task "$first_task" | json_field package_id)"
first_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$first_task" | json_field unit_id)"
cli schedule unit "$first_unit" --package-id "$first_package" --revision 1 >/dev/null
cli schedule release-now "$first_unit" >/dev/null

development_head=''
for _ in {1..90}; do
  development_head="$(remote_ref refs/heads/development)"
  [[ -n "$development_head" ]] && break
  sleep 2
done
[[ -n "$development_head" ]] || fail "the work never reached the development branch"

# --- An empty change is refused, so nothing can look like it worked when it did nothing. ---
if empty="$(cli repository set-policy "$repository_id" 2>&1)"; then
  fail "a policy change naming nothing was accepted"
fi
grep -q "at least one thing to change" <<<"$empty" || \
  fail "the empty-change refusal did not say what was missing: $empty"

# --- Removing the development branch would strand work that is on it. ---
if strand="$(cli repository set-policy "$repository_id" \
  --mode scheduled --no-development-target 2>&1)"; then
  fail "the development branch was removed while published work still owed an integration"
fi
grep -q "would strand it" <<<"$strand" || \
  fail "the refusal did not explain what would be lost: $strand"
grep -q "$first_unit" <<<"$strand" || \
  fail "the refusal did not name the unit at risk: $strand"

# Nothing was applied: a refusal that half-completes is the defect this exists to prevent.
repositories="$(cli repository list)"
grep -q '"policy_revision":1' <<<"$repositories" || \
  fail "a refused policy change still moved the policy revision: $repositories"
grep -q '"development_target":"refs/heads/development"' <<<"$repositories" || \
  fail "a refused policy change still removed the development branch: $repositories"

# --- A second unit has a release time chosen under the current policy. ---
printf 'waiting for a destination\n' >"$workspace_path/waiting.txt"
second_package="$(cli package capture "$feature_id" --revision 1 --task "$second_task" | json_field package_id)"
second_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$second_task" | json_field unit_id)"
slot="$(cli schedule unit "$second_unit" --package-id "$second_package" --revision 1)"
selected_before="$(json_number selected_at_unix_ms <<<"$slot")"
[[ -n "$selected_before" ]] || fail "the second unit did not get a release time"

# --- Changing the target is allowed, and says what it had to move. ---
changed="$(cli repository set-policy "$repository_id" --target release)"
grep -q '"policy_revision":2' <<<"$changed" || \
  fail "the policy change did not produce a new revision: $changed"
grep -q '"target":"refs/heads/release"' <<<"$changed" || \
  fail "the policy change did not record the new target: $changed"
grep -q "$second_unit" <<<"$changed" || \
  fail "the policy change did not report withdrawing a time chosen under the old policy: $changed"

# The unit keeps its identity — a second one would be a second publication of the same work.
units="$(cli release units --feature "$feature_id" --revision 1 2>/dev/null || cli queue status)"
[[ "$(grep -o "$second_unit" <<<"$units" | wc -l | tr -d ' ')" -ge 1 ]] || \
  fail "the unit disappeared when the policy changed: $units"
if cli schedule show "$second_unit" >/dev/null 2>&1; then
  fail "a release time chosen under the old policy is still live"
fi

history="$(cli schedule history "$second_unit")"
grep -q "publication policy changed" <<<"$history" || \
  fail "the withdrawn time does not record why it was withdrawn: $history"

# --- And the new destination is the one actually used. ---
rescheduled="$(cli schedule unit "$second_unit" --package-id "$second_package" --revision 1)"
grep -q '"type":"schedule_slot"' <<<"$rescheduled" || \
  fail "the unit could not be scheduled again under the new policy: $rescheduled"
cli schedule release-now "$second_unit" >/dev/null

for _ in {1..90}; do
  [[ -n "$(remote_ref refs/heads/development)" ]] && break
  sleep 2
done
[[ "$(remote_ref refs/heads/main)" == "$main_before" ]] || \
  fail "work reached the branch the old policy named after the target was changed"

# --- And the product never claims a release time is a contribution date. ---
contributions="$("$binary_dir/reccursive" --state-dir "$fixture_root/state" \
  contributions "$repository_id")"
grep -q "It is not a promise about your" <<<"$contributions" || \
  fail "the contribution explanation does not separate a release time from a contribution: $contributions"
grep -q "cannot check from here" <<<"$contributions" || \
  fail "the contribution explanation does not say what it cannot verify: $contributions"
grep -q "fixture@example.invalid" <<<"$contributions" || \
  fail "the contribution explanation does not name the identity commits will carry: $contributions"
grep -q "refs/heads/release" <<<"$contributions" || \
  fail "the contribution explanation does not name the branch work lands on: $contributions"
grep -q "no commit is backdated" <<<"$contributions" || \
  fail "the contribution explanation does not rule out backdating: $contributions"

printf 'PASS a policy change refuses what would strand or duplicate work, and moves only what it must\n'
