#!/usr/bin/env bash
# Proves every blocking failure leaves a supported next action, that diagnostics can be shared
# without giving up the queue, and that an operator can stop one repository or all of them.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8e.XXXXXX")"
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

make_repository() {
  local name="$1"
  git init --quiet --bare --initial-branch=main "$fixture_root/$name-remote.git"
  git init --quiet --initial-branch=main "$fixture_root/$name"
  git -C "$fixture_root/$name" config user.name "Scenario Fixture"
  git -C "$fixture_root/$name" config user.email "fixture@example.invalid"
  printf '# %s\n' "$name" >"$fixture_root/$name/README.md"
  git -C "$fixture_root/$name" add README.md
  git -C "$fixture_root/$name" commit --quiet -m "Initialize $name"
  git -C "$fixture_root/$name" remote add origin "$fixture_root/$name-remote.git"
  git -C "$fixture_root/$name" push --quiet -u origin main
  cli setup "$fixture_root/$name" --non-interactive --timezone UTC | json_field repository_id
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

start_daemon
first_repository="$(make_repository alpha)"
second_repository="$(make_repository beta)"

# ---- A blocked attempt names what to do about it ----

feature_id="feature_00000000-0000-4000-8000-000000001001"
task_id="task_00000000-0000-4000-8000-000000001002"
cat >"$fixture_root/plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$first_repository",
  "goal": "Fail in a way that says what to do",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Work that cannot reach its remote","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
    ]
  }]
}
JSON
cli plan import "$fixture_root/plan.json" >/dev/null
workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'blocked work\n' >"$workspace_path/blocked.txt"
submission="$(cli task submit "$feature_id" --revision 1 --task "$task_id")"
package_id="$(json_field package_id <<<"$submission")"
# The queue identifies work by release unit, not by package.
unit_id="$(json_field unit_id <<<"$submission")"

# Removing the remote makes a real publication fail for a real reason.
mv "$fixture_root/alpha-remote.git" "$fixture_root/alpha-remote-gone.git"
attempt_id="$(cli release publish "$package_id" --revision 1 --message "Publish blocked work" \
  --author-name "Scenario Fixture" --author-email fixture@example.invalid | json_field attempt_id)"
[[ "$attempt_id" == attempt_* ]] || fail "no attempt was recorded for the failed publication"

attempt="$(plain release attempt "$attempt_id")"
grep -q "^Next:" <<<"$attempt" || \
  fail "a blocked attempt offered no next action: $attempt"
grep -q "reccursive" <<<"$attempt" || \
  fail "the next action names no command: $attempt"
mv "$fixture_root/alpha-remote-gone.git" "$fixture_root/alpha-remote.git"

# ---- Diagnostics can be shared, and exporting them costs nothing ----

queue_before="$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')"
attempts_before="$(cli release attempts --limit 20)"

plain diagnostics export "$fixture_root/report" >/dev/null
for section in manifest.json status.json repositories.json queue.json integrations.json events.json; do
  [[ -f "$fixture_root/report/$section" ]] || fail "the diagnostics report is missing $section"
done
grep -q '"kind":"reccursive-diagnostics"' "$fixture_root/report/manifest.json" || \
  fail "the report has no manifest identifying it"

# The report must not carry the one secret this installation has.
auth_token="$(cat "$fixture_root/state/auth.token")"
if grep -rqF "$auth_token" "$fixture_root/report" 2>/dev/null; then
  fail "the diagnostics report contains the local API authentication token"
fi
[[ ! -f "$fixture_root/report/state.sqlite" ]] || fail "the report copied the queue database"

# Exporting describes the queue; it must never consume it.
[[ "$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')" == "$queue_before" ]] || \
  fail "exporting diagnostics changed the queue"
[[ "$(cli release attempts --limit 20)" == "$attempts_before" ]] || \
  fail "exporting diagnostics changed the durable attempt record"

# Writing over an existing directory is refused rather than merged into.
plain diagnostics export "$fixture_root/report" >"$fixture_root/reexport.txt" 2>&1 && \
  fail "exporting over an existing directory was accepted"

# ---- Filters narrow the queue to what a person has to look at ----

blocked_only="$(cli queue status --needs-attention)"
grep -q "$unit_id" <<<"$blocked_only" || \
  fail "--needs-attention hid a blocked unit: $blocked_only"
published_only="$(cli queue status --state published)"
grep -q "$unit_id" <<<"$published_only" && \
  fail "--state published matched a unit that never published: $published_only"

# ---- One repository, or every repository, explicitly ----

cli schedule pause "$second_repository" --reason "pausing just this one" >/dev/null
one_paused="$(plain queue status)"
grep -q "pausing just this one" <<<"$one_paused" || fail "pausing one repository was not reported"
cli schedule resume "$second_repository" >/dev/null

# Naming no repository and not asking for all of them is a usage error, not a guess.
cli schedule pause --reason "ambiguous" >"$fixture_root/ambiguous.json" 2>&1 && \
  fail "pausing without naming a repository or --all was accepted"
grep -q '"code":"repository_required"' "$fixture_root/ambiguous.json" || \
  fail "an ambiguous pause was not refused clearly: $(cat "$fixture_root/ambiguous.json")"

paused_all="$(cli schedule pause --all --reason "stopping everything for a review")"
grep -q '"type":"repositories_paused"' <<<"$paused_all" || \
  fail "pausing every repository was not reported: $paused_all"
for repository in "$first_repository" "$second_repository"; do
  grep -q "$repository" <<<"$paused_all" || fail "$repository was not paused by --all"
done
all_paused_queue="$(plain queue status)"
[[ "$(grep -c "stopping everything for a review" <<<"$all_paused_queue")" == "2" ]] || \
  fail "both repositories do not report being paused: $all_paused_queue"

# Pausing stops new releases; it does not discard queued work.
grep -q "$unit_id" <<<"$(cli queue status)" || fail "pausing discarded queued work"

printf 'PASS blocked work names its next action, diagnostics export without cost, and repositories pause one by one or all at once\n'
