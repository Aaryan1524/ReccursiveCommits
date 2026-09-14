#!/usr/bin/env bash
# Walks the whole product the way a new user does, using only documented commands, and checks the
# properties a terminal tool has to hold while doing it: no prompts without a terminal, no control
# characters in piped output, no dependence on a wide screen, and service work that survives the
# terminal closing.
#
# Every other scenario proves one capability. This one proves they compose.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8f.XXXXXX")"
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
    >>"$fixture_root/daemon.log" 2>&1 &
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

# Output a person reads must never contain terminal control characters when it is not going to a
# terminal — piping into a file, a pager, or another program has to produce plain text.
assert_plain_text() {
  local description="$1" file="$2"
  if LC_ALL=C grep -q $'\033' "$file"; then
    fail "$description emitted terminal escape sequences into piped output"
  fi
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Journey fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon

# ---- 1. Onboarding, as documented ----

plain doctor >"$fixture_root/doctor.txt" 2>&1 || true
assert_plain_text "doctor" "$fixture_root/doctor.txt"

repository_id="$(cli setup "$fixture_root/repository" --non-interactive --timezone UTC | json_field repository_id)"
[[ "$repository_id" == repo_* ]] || fail "onboarding did not enroll a repository"

# ---- 2. Authoring and approving a plan ----

plain plan template "$repository_id" --target main -o "$fixture_root/plan.json" >/dev/null
plain plan check "$fixture_root/plan.json" >"$fixture_root/check.txt"
assert_plain_text "plan check" "$fixture_root/check.txt"
feature_ids="$(grep -o '"feature_id": "[^"]*"' "$fixture_root/plan.json")"
feature_id="$(sed -E 's/.*"feature_id": "([^"]*)".*/\1/' <<<"${feature_ids%%$'\n'*}")"
cli plan import "$fixture_root/plan.json" >/dev/null

# The template's two tasks are sequential, so only the first can be worked on now.
plain plan show "$feature_id" >"$fixture_root/review.txt"
assert_plain_text "plan show" "$fixture_root/review.txt"
grep -q "waits for" "$fixture_root/review.txt" || fail "the review does not show the dependency"

sealed_revision="$(cli plan seal "$feature_id" | sed -E 's/.*"revision":([0-9]+).*/\1/')"
[[ "$sealed_revision" == "2" ]] || fail "sealing did not append a revision: $sealed_revision"

# ---- 3. Doing and submitting the work ----

workspace_path="$(cli workspace create "$feature_id" --revision "$sealed_revision" | json_field path)"
sealed_plan="$(cli plan show "$feature_id" --revision "$sealed_revision")"
task_ids="$(grep -o '"id":"task_[^"]*"' <<<"$sealed_plan")"
first_task="$(sed -E 's/.*"(task_[^"]*)".*/\1/' <<<"${task_ids%%$'\n'*}")"
printf 'the first unit of work\n' >"$workspace_path/first.txt"
submission="$(cli task submit "$feature_id" --revision "$sealed_revision" --task "$first_task")"
unit_id="$(json_field unit_id <<<"$submission")"
package_id="$(json_field package_id <<<"$submission")"

# ---- 4. Inspecting it before it goes anywhere ----

plain queue status >"$fixture_root/queue.txt"
assert_plain_text "queue status" "$fixture_root/queue.txt"
grep -q "$unit_id" "$fixture_root/queue.txt" || fail "the submitted unit is not in the queue"

plain package changes "$package_id" >"$fixture_root/changes.txt"
grep -q "first.txt" "$fixture_root/changes.txt" || fail "the captured change is not inspectable"
plain package checks "$package_id" >"$fixture_root/checks.txt"
grep -q "passed" "$fixture_root/checks.txt" || fail "the checks that ran are not readable"

# A narrow terminal must not break anything. Every column is capped and newlines are stripped, so
# a 40-column screen wraps rather than producing unreadable output or an error.
COLUMNS=40 plain queue status >"$fixture_root/narrow.txt" 2>&1 || \
  fail "queue status failed on a narrow terminal"
assert_plain_text "queue status at 40 columns" "$fixture_root/narrow.txt"
grep -q "$unit_id" "$fixture_root/narrow.txt" || fail "a narrow terminal lost the unit"

# ---- 5. Queue controls ----

cli schedule pause "$repository_id" --reason "reviewing before release" >/dev/null
grep -q "reviewing before release" <<<"$(plain queue status)" || fail "the pause is not visible"
cli schedule resume "$repository_id" >/dev/null

cli schedule withdraw "$unit_id" --reason "changed my mind about the timing" >/dev/null
withdrawn_queue="$(cli queue status)"
grep -q "changed my mind about the timing" <<<"$withdrawn_queue" || \
  fail "the queue does not explain the withdrawn release time"
cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 >/dev/null

# ---- 6. Closing and reopening the terminal ----

# Every CLI process ends; the daemon is a separate process and keeps its work. This is what the
# installed service does when a terminal window closes.
before_restart="$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')"
kill -0 "$daemon_pid" 2>/dev/null || fail "the daemon was not running before the terminal closed"
after_reopen="$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')"
[[ "$before_restart" == "$after_reopen" ]] || fail "reopening the terminal changed the queue"

# And the stronger case: the service itself restarts, as it would after a reboot.
stop_daemon
start_daemon
recovered="$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')"
[[ "$recovered" == "$before_restart" ]] || \
  fail "restarting the service lost or changed queued work:\n$before_restart\n---\n$recovered"
cli schedule show "$unit_id" >/dev/null || fail "the selected release time did not survive a restart"

# ---- 7. Error recovery, end to end ----

# Make a real publication fail, follow the advice the product gives, and confirm recovery.
mv "$fixture_root/remote.git" "$fixture_root/remote-gone.git"
attempt_id="$(cli release publish "$package_id" --revision 1 --message "Publish the first unit" \
  --author-name "Scenario Fixture" --author-email fixture@example.invalid | json_field attempt_id)"
plain release attempt "$attempt_id" >"$fixture_root/blocked.txt"
assert_plain_text "release attempt" "$fixture_root/blocked.txt"
grep -q "^Next:" "$fixture_root/blocked.txt" || \
  fail "a blocked attempt gave no next action: $(cat "$fixture_root/blocked.txt")"

# Asking for help must not cost the queue.
queue_before_export="$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')"
plain diagnostics export "$fixture_root/report" >/dev/null
[[ "$(cli queue status | sed -E 's/"generated_at_unix_ms":[0-9]+,?//')" == "$queue_before_export" ]] || \
  fail "exporting diagnostics changed the queue"

# Fix the cause and publish for real.
mv "$fixture_root/remote-gone.git" "$fixture_root/remote.git"
cli release publish "$package_id" --revision 1 --message "Publish the first unit" \
  --author-name "Scenario Fixture" --author-email fixture@example.invalid \
  >"$fixture_root/published.json" 2>&1 || \
  fail "publication did not succeed after the cause was fixed: $(cat "$fixture_root/published.json")"

git clone --quiet "$fixture_root/remote.git" "$fixture_root/verification"
[[ -f "$fixture_root/verification/first.txt" ]] || fail "the work did not reach the remote"
published_subjects="$(git -C "$fixture_root/verification" log --format=%s)"
grep -Fxq "Publish the first unit" <<<"$published_subjects" || \
  fail "the published commit does not carry its message: $published_subjects"

# ---- 8. Nothing along the way needed a terminal, a mouse, or a full screen ----

# Watching is the only live view, and piped it must stay plain text.
plain queue watch --interval 1 --for 2 >"$fixture_root/watch.txt" 2>&1
assert_plain_text "queue watch" "$fixture_root/watch.txt"

# The user's own checkout is untouched by the entire journey.
[[ -z "$(git -C "$fixture_root/repository" status --porcelain)" ]] || \
  fail "the journey dirtied the user's checkout"

printf 'PASS a user completed onboarding, approval, submission, inspection, control, restart, and recovery through documented commands alone\n'
