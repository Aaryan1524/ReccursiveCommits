#!/usr/bin/env bash
# Sets up the Phase 5 exit test, which needs a real sleep and so cannot be automated.
#
# This script does every part that can be done for you: it builds a throwaway repository and bare
# remote, installs the service, enrolls the repository, imports a plan, captures a unit, and
# schedules that unit for a time far enough out that the machine can be asleep when it arrives.
#
# Then it stops and tells you what to do by hand. Nothing here touches any real repository.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
root="${RECCURSIVE_SLEEP_TEST_DIR:-$HOME/reccursive-sleep-test}"
minutes="${RECCURSIVE_SLEEP_MINUTES:-12}"

cli() { "$binary_dir/reccursive" --state-dir "$root/state" "$@"; }
cli_json() { "$binary_dir/reccursive" --state-dir "$root/state" --json "$@"; }
json_field() { sed -E "s/.*\"$1\":\"([^\"]+)\".*/\1/"; }

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

if [[ "${1:-setup}" == "check" ]]; then
  # ---- Run this after waking the machine. ----
  [[ -d "$root" ]] || fail "no test fixture at $root; run this script without arguments first"
  head_before="$(cat "$root/head-before.txt")"

  # The maintenance pass runs on an interval, and while the machine is asleep its timer is
  # suspended — so after a wake the publish lands shortly *after* you get back to the keyboard,
  # not at the scheduled time. Checking once and declaring failure turns "not yet" into "broken",
  # so wait out a couple of passes before saying anything.
  printf '\n=== Phase 5 sleep test result ===\n\n'
  head_now="$(git -C "$root/remote.git" rev-parse refs/heads/main)"
  if [[ "$head_now" == "$head_before" ]]; then
    printf 'Nothing published yet. Waiting up to 3 minutes for the next maintenance pass'
    for _ in {1..36}; do
      sleep 5
      printf '.'
      head_now="$(git -C "$root/remote.git" rev-parse refs/heads/main)"
      [[ "$head_now" != "$head_before" ]] && break
    done
    printf '\n\n'
  fi

  if [[ "$head_now" == "$head_before" ]]; then
    printf 'NOT PUBLISHED — the remote is still unchanged after waiting.\n\n'
    printf 'Where to look:\n'
    cli schedule preview "$(cat "$root/repository-id.txt")" || true
    printf '\n'
    cli integrations || true
    printf '\n'
    cli diagnose "$(cat "$root/repository-id.txt")" || true
    printf '\nService log (last 30 lines):\n'
    tail -n 30 "$root/state/service.log" 2>/dev/null || printf '  (no log yet)\n'
    exit 1
  fi

  printf 'PUBLISHED while you were away.\n\n'
  git -C "$root/remote.git" log -1 --format='  commit:  %H%n  subject: %s%n  author:  %an <%ae>%n  date:    %ad'
  printf '\nThe daemon did this on its own: no release command was issued after setup.\n\n'
  printf 'Attempt record:\n'
  cli release attempts --limit 5 || true
  printf '\nWhen you are done:\n'
  printf '  %s --state-dir %s service uninstall\n' "$binary_dir/reccursive" "$root/state"
  printf '  rm -rf %s\n' "$root"
  exit 0
fi

# ---- Setup ----
command -v git >/dev/null || fail "git is not installed"
[[ -x "$binary_dir/reccursive" && -x "$binary_dir/reccursive-daemon" ]] || {
  printf 'Building...\n'
  (cd "$project_root" && cargo build --workspace --locked)
}

if [[ -e "$root" ]]; then
  fail "$root already exists; remove it first or set RECCURSIVE_SLEEP_TEST_DIR"
fi
mkdir -p "$root"

printf 'Creating a throwaway repository and remote in %s\n' "$root"
git init --quiet --bare --initial-branch=main "$root/remote.git"
git init --quiet --initial-branch=main "$root/repository"
git -C "$root/repository" config user.name "$(git config --global user.name || echo 'Sleep Test')"
git -C "$root/repository" config user.email "$(git config --global user.email || echo 'sleep-test@example.invalid')"
printf '# Sleep test\n' >"$root/repository/README.md"
git -C "$root/repository" add README.md
git -C "$root/repository" commit --quiet -m "Initialize sleep test"
git -C "$root/repository" remote add origin "$root/remote.git"
git -C "$root/repository" push --quiet -u origin main
git -C "$root/remote.git" rev-parse refs/heads/main >"$root/head-before.txt"

printf 'Installing the service...\n'
cli service install >/dev/null
for _ in {1..100}; do
  cli_json status >/dev/null 2>&1 && break
  sleep 0.2
done
cli_json status >/dev/null 2>&1 || fail "the installed service did not come up; see $root/state/service.log"

repository_id="$(cli_json repository add "$root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "enrollment did not return a repository id"
printf '%s' "$repository_id" >"$root/repository-id.txt"

feature_id="feature_00000000-0000-4000-8000-0000000005f1"
task_id="task_00000000-0000-4000-8000-0000000005f2"
cat >"$root/plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Publish while the machine is asleep",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Publish after a real sleep","dependencies":{},"acceptance_checks":[{"id":"present","description":"the change reaches the remote"}]}
    ]
  }]
}
JSON
cli_json plan import "$root/plan.json" >/dev/null

workspace_path="$(cli_json workspace create "$feature_id" --revision 1 | json_field path)"
printf 'written while the laptop lid was shut\n' >"$workspace_path/slept.txt"
package_id="$(cli_json package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
unit_id="$(cli_json release create-unit "$feature_id" --revision 1 --task "$task_id" | json_field unit_id)"

# A deliberately narrow window starting `minutes` from now, in the machine's own time zone.
#
# Narrow matters. A policy picks a random time *within* its window, which is exactly what you want
# in real use and exactly what you do not want in a test: a window running to end-of-day would
# schedule this for some unpredictable hour, and you would sleep the machine and see nothing for
# hours through no fault of the service.
timezone="$(readlink /etc/localtime 2>/dev/null | sed 's|.*/zoneinfo/||')"
timezone="${timezone:-UTC}"
start_h="$(date -v +"${minutes}"M +%H 2>/dev/null || date -d "+${minutes} minutes" +%H)"
start_m="$(date -v +"${minutes}"M +%M 2>/dev/null || date -d "+${minutes} minutes" +%M)"
start_total=$((10#$start_h * 60 + 10#$start_m))
end_total=$((start_total + 5))
if (( end_total >= 24 * 60 )); then
  fail "the test window would cross midnight; run this earlier in the day, or set RECCURSIVE_SLEEP_MINUTES smaller"
fi
cat >"$root/policy.json" <<JSON
{
  "timezone": "$timezone",
  "allowed_days": ["monday","tuesday","wednesday","thursday","friday","saturday","sunday"],
  "windows": [{"start":{"hour":$((start_total / 60)),"minute":$((start_total % 60))},"end":{"hour":$((end_total / 60)),"minute":$((end_total % 60))}}],
  "daily_releases": {"minimum": 1, "maximum": 5},
  "minimum_spacing_minutes": 1,
  "missed_window_behavior": {"kind": "catch_up", "max_releases": 3}
}
JSON
cli_json schedule set-policy "$repository_id" "$root/policy.json" >/dev/null
cli_json schedule unit "$unit_id" --package-id "$package_id" --revision 1 >/dev/null

printf '\n=== Ready ===\n\n'
printf 'Repository: %s\n' "$repository_id"
printf 'Time zone:  %s\n' "$timezone"
printf 'Scheduled:  '
cli schedule preview "$repository_id"
selected_ms="$(cli_json schedule show "$unit_id" | sed -E 's/.*"selected_at_unix_ms":([0-9]+).*/\1/')"
if [[ -n "$selected_ms" ]]; then
  printf 'That is:    '
  date -r "$((selected_ms / 1000))" "+%Y-%m-%d %H:%M %Z" 2>/dev/null \
    || date -d "@$((selected_ms / 1000))" "+%Y-%m-%d %H:%M %Z"
fi
printf '\nCredentials and signing right now:\n'
cli diagnose "$repository_id"

cat <<INSTRUCTIONS

=== What to do by hand ===

  1. Close this terminal completely. The service is a launchd agent, so it keeps
     running — that is part of what is being tested.

  2. Put the Mac to sleep, and leave it asleep past the scheduled time above
     (at least ${minutes} minutes from now).

  3. Wake it and run:

       $0 check

     Run it whenever you like — it waits out a couple of maintenance passes before
     reporting, because the publish lands shortly after you wake the machine rather
     than at the scheduled time. While the Mac is asleep the pass's timer is
     suspended, so the work is caught on wake; that is the behaviour being tested.

If it still reports nothing after waiting, it shows you the schedule, integration
health, credential state and service log so the reason is visible rather than
guessed at.

Nothing here touches any repository of yours. To remove it all:

  $binary_dir/reccursive --state-dir $root/state service uninstall
  rm -rf $root

INSTRUCTIONS
