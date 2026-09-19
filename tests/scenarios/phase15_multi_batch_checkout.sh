#!/usr/bin/env bash
# Drives the real, interactive `reccursive schedule` wizard on a pseudo-terminal and proves the
# multi-batch checkout flow end to end: splitting one checkout into two independently-timed
# batches, leaving the rest unscheduled, and having each batch publish exactly what it captured
# even after the checkout keeps changing underneath it.
#
# Every other checkout scenario drives the records the wizard builds through the commands it
# composes, because the wizard itself needs a real terminal. This one gives it one, since the
# multi-batch session loop, the pending-sibling time validation, and the all-or-nothing plan
# confirmation are wizard-side behaviour with no equivalent direct command to stand in for them.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase15.XXXXXX")"
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
  [[ -f "$fixture_root/transcript.txt" ]] && {
    printf -- '---- wizard transcript ----\n' >&2
    cat "$fixture_root/transcript.txt" >&2
  }
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
cli() { "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json "$@"; }

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

# --- Fixture: a repository with six changes in its checkout, matching the acceptance spec. ---
git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

printf 'feed v1\n' >"$fixture_root/repository/feed.rs"
printf 'feed test v1\n' >"$fixture_root/repository/feed_test.rs"
printf 'auth v1\n' >"$fixture_root/repository/auth.rs"
printf 'auth test v1\n' >"$fixture_root/repository/auth_test.rs"
git -C "$fixture_root/repository" add feed.rs feed_test.rs auth.rs auth_test.rs
git -C "$fixture_root/repository" commit --quiet -m "Base files"
git -C "$fixture_root/repository" push --quiet origin main

# Now dirty the checkout: M feed.rs, M feed_test.rs, M auth.rs, M auth_test.rs, M README.md, and
# an untracked notes.txt — the exact shape the acceptance spec asks for.
printf 'feed v2\n' >"$fixture_root/repository/feed.rs"
printf 'feed test v2\n' >"$fixture_root/repository/feed_test.rs"
printf 'auth v2\n' >"$fixture_root/repository/auth.rs"
printf 'auth test v2\n' >"$fixture_root/repository/auth_test.rs"
printf '# Fixture\nmore docs\n' >"$fixture_root/repository/README.md"
printf 'scratch notes\n' >"$fixture_root/repository/notes.txt"
status_before="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"

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

# Near enough that the scenario does not sit for long, far enough to survive the wizard's own
# interaction time and clear the "requested time is in the future" check at both validation and
# persistence. Two minutes apart clears the policy's minimum spacing with room to spare.
batch_one_at="$(date -u -v +90S '+%Y-%m-%d %H:%M' 2>/dev/null || date -u -d '+90 seconds' '+%Y-%m-%d %H:%M')"
batch_two_at="$(date -u -v +210S '+%Y-%m-%d %H:%M' 2>/dev/null || date -u -d '+210 seconds' '+%Y-%m-%d %H:%M')"
batch_one_date="${batch_one_at% *}"
batch_one_time="${batch_one_at#* }"
batch_two_date="${batch_two_at% *}"
batch_two_time="${batch_two_at#* }"

python3 - "$fixture_root/wizard_steps.json" "$batch_one_date" "$batch_one_time" \
  "$batch_two_date" "$batch_two_time" <<'PY'
import json
import sys

_, out_path, one_date, one_time, two_date, two_time = sys.argv
ESC = chr(27)
DOWN = ESC + "[B"

# Files sort alphabetically: README.md, auth.rs, auth_test.rs, feed.rs, feed_test.rs, notes.txt.
# Batch one claims feed.rs and feed_test.rs (indices 3, 4); batch two then claims auth.rs and
# auth_test.rs, which are indices 1 and 2 of the four files still remaining.
steps = [
    {"expect": "Which files belong"},
    {"raw": DOWN}, {"raw": DOWN}, {"raw": DOWN}, {"raw": " "},
    {"raw": DOWN}, {"raw": " "},
    {"raw": "\r"},
    {"expect": "What should this change be called"},
    {"send": "Personalized feed ranking"},
    {"expect": "Date"},
    {"send": one_date},
    {"expect": "Time"},
    {"send": one_time},
    {"expect": "What next"},
    {"raw": "\r"},
    {"expect": "Which files belong"},
    {"raw": DOWN}, {"raw": " "},
    {"raw": DOWN}, {"raw": " "},
    {"raw": "\r"},
    {"expect": "What should this change be called"},
    {"send": "Auth cleanup"},
    {"expect": "Date"},
    {"send": two_date},
    {"expect": "Time"},
    {"send": two_time},
    {"expect": "What next"},
    {"raw": DOWN},
    {"raw": "\r"},
    {"expect": "Schedule these"},
    {"raw": "\r"},
    {"expect": "releases scheduled|Not scheduled"},
]
with open(out_path, "w") as handle:
    json.dump(steps, handle)
PY

(
  cd "$fixture_root/repository"
  python3 "$project_root/tests/scenarios/support/pty_driver.py" "$fixture_root/wizard_steps.json" \
    -- "$binary_dir/reccursive" --state-dir "$fixture_root/state" schedule \
    >"$fixture_root/transcript.txt" 2>&1
) || fail "the wizard did not complete the scripted session"

grep -q "2 releases scheduled" "$fixture_root/transcript.txt" || \
  fail "the wizard did not report two releases scheduled"
grep -q "Personalized feed ranking" "$fixture_root/transcript.txt" || \
  fail "the first batch is missing from the confirmation"
grep -q "Auth cleanup" "$fixture_root/transcript.txt" || \
  fail "the second batch is missing from the confirmation"

# --- Before either release fires: edit a scheduled file again, and add a new one. ---
printf 'feed v3 -- edited after scheduling\n' >"$fixture_root/repository/feed.rs"
printf 'added after scheduling\n' >"$fixture_root/repository/added_after.txt"
head_after_edit="$(git -C "$fixture_root/repository" rev-parse HEAD)"

# --- Both releases must publish naturally, each with exactly its own two files. ---
#
# `feed.rs` and `auth.rs` were already present on the target from the "Base files" commit made
# while building the fixture, so a check that only asks whether the path exists on the target
# would pass before either scheduled release has actually landed. What each batch adds is a new
# commit carrying its own name, so that name — not the path — is what a wait has to look for.
wait_for_commit_message() {
  local message="$1"
  for _ in {1..90}; do
    if git -C "$fixture_root/remote.git" log refs/heads/main --format=%H --grep="^$message\$" \
        | grep -q .; then
      return 0
    fi
    sleep 2
  done
  fail "no commit named '$message' was ever published"
}
wait_for_commit_message "Personalized feed ranking"
wait_for_commit_message "Auth cleanup"

feed_commit="$(git -C "$fixture_root/remote.git" log refs/heads/main --format=%H \
  --grep='^Personalized feed ranking$')"
auth_commit="$(git -C "$fixture_root/remote.git" log refs/heads/main --format=%H \
  --grep='^Auth cleanup$')"

# Batch one carries exactly its two captured files, at the version captured — not the later edit.
feed_stat="$(git -C "$fixture_root/remote.git" show --stat --format='' "$feed_commit")"
grep -q "feed.rs" <<<"$feed_stat" || fail "batch one's commit does not touch feed.rs"
grep -q "feed_test.rs" <<<"$feed_stat" || fail "batch one's commit does not touch feed_test.rs"
grep -qE "auth\.rs|auth_test\.rs|notes\.txt|added_after\.txt|README\.md" <<<"$feed_stat" && \
  fail "batch one's commit carries a file it was never assigned: $feed_stat"
[[ "$(git -C "$fixture_root/remote.git" show "$feed_commit":feed.rs)" == "feed v2" ]] || \
  fail "batch one published the post-scheduling edit instead of its captured snapshot"

# Batch two carries exactly its two captured files, independent of batch one.
auth_stat="$(git -C "$fixture_root/remote.git" show --stat --format='' "$auth_commit")"
grep -q "auth.rs" <<<"$auth_stat" || fail "batch two's commit does not touch auth.rs"
grep -q "auth_test.rs" <<<"$auth_stat" || fail "batch two's commit does not touch auth_test.rs"
grep -qE "feed\.rs|feed_test\.rs|notes\.txt|added_after\.txt|README\.md" <<<"$auth_stat" && \
  fail "batch two's commit carries a file it was never assigned: $auth_stat"
[[ "$(git -C "$fixture_root/remote.git" show "$auth_commit":auth.rs)" == "auth v2" ]] || \
  fail "batch two published the wrong content"

# Neither unscheduled file, nor the file added after scheduling, ever reached the target.
git -C "$fixture_root/remote.git" show refs/heads/main:notes.txt >/dev/null 2>&1 && \
  fail "an unscheduled file was published"
git -C "$fixture_root/remote.git" show refs/heads/main:added_after.txt >/dev/null 2>&1 && \
  fail "a file added after scheduling was published"
readme_published="$(git -C "$fixture_root/remote.git" show refs/heads/main:README.md)"
[[ "$readme_published" == "# Fixture" ]] || \
  fail "the unscheduled README edit was published: $readme_published"

# --- The user's checkout itself was never touched. ---
[[ "$(git -C "$fixture_root/repository" rev-parse HEAD)" == "$head_after_edit" ]] || \
  fail "the user's HEAD moved"
[[ -z "$(git -C "$fixture_root/repository" stash list)" ]] || \
  fail "something stashed the user's work"
status_after="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"
[[ "$(cat "$fixture_root/repository/feed.rs")" == "feed v3 -- edited after scheduling" ]] || \
  fail "the post-scheduling edit to feed.rs was overwritten"
grep -q "added_after.txt" <<<"$status_after" || \
  fail "the file added after scheduling vanished from the checkout"

# --- queue status shows both as published releases, each on the target with its own commit. ---
status_json="$(cli queue status)"
grep -q "\"release_unit_id\"" <<<"$status_json" || fail "queue status reported no units"
[[ "$(grep -o '"state":"published"' <<<"$status_json" | wc -l | tr -d ' ')" == "2" ]] || \
  fail "queue status does not show both releases published: $status_json"

printf 'ok\n'
