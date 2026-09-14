#!/usr/bin/env bash
# Proves the pull-request strategy end to end, and proves the one line it must never cross.
#
# The whole appeal of this strategy is that the commit and the pull request happen on a schedule
# while you are not there, and all that is left for you is the merge. So the claims that matter
# are: a pull request really is opened without anyone asking, the target branch is not touched
# while it waits, the work is not reported as finished, and merging is the only thing that ever
# finishes it — done by a person, never by this product.
#
# The adapter, curl, and the daemon are all real. Only GitHub is a stand-in, served over a Unix
# socket, so there is no separate code path that could pass here while the real one is broken.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase10-pr.XXXXXX")"
daemon_pid=''
github_pid=''
github_socket="$fixture_root/github.sock"

stop_processes() {
  for pid in "$daemon_pid" "$github_pid"; do
    if [[ -n "$pid" ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  daemon_pid=''
  github_pid=''
}

cleanup() {
  stop_processes
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

command -v python3 >/dev/null || fail "python3 is required to serve the stand-in endpoint"

start_github() {
  python3 "$project_root/tests/scenarios/support/fake_github.py" "$github_socket" \
    >"$fixture_root/github.log" 2>&1 &
  github_pid=$!
  for _ in {1..100}; do
    [[ -S "$github_socket" ]] && return 0
    kill -0 "$github_pid" 2>/dev/null || fail "the stand-in endpoint exited during startup"
    sleep 0.05
  done
  fail "the stand-in endpoint was not ready in time"
}

start_daemon() {
  # The remote really is a GitHub URL, because that is what the slug is read from and a fixture
  # that faked it would not be testing the same thing. Git rewrites it to the local bare
  # repository through a config file of our own, so pushes stay here and nothing real is touched:
  # GIT_CONFIG_GLOBAL replaces the user's ~/.gitconfig for these processes only.
  GIT_CONFIG_GLOBAL="$fixture_root/gitconfig" \
  RECCURSIVE_GITHUB_SOCKET="$github_socket" \
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

plain() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" "$@"
}

github_control() {
  curl --silent --show-error --unix-socket "$github_socket" --request POST \
    "http://localhost$1"
}

github_pulls() {
  curl --silent --show-error --unix-socket "$github_socket" "http://localhost/control/pulls"
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
printf '# Pull request fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
main_before="$(remote_ref refs/heads/main)"

cat >"$fixture_root/gitconfig" <<EOF
[url "$fixture_root/remote.git"]
	insteadOf = https://github.com/owner/project.git
EOF

start_github
start_daemon

# --- A repository whose remote is not GitHub cannot use this strategy, and is told so. ---
if wrong_host="$(cli repository add "$fixture_root/repository" \
  --mode immediate --development-target development \
  --integration pull-request 2>&1)"; then
  fail "the pull-request strategy was accepted for a remote GitHub does not host"
fi
grep -q "needs a GitHub remote" <<<"$wrong_host" || \
  fail "the refusal did not explain why the remote cannot be used: $wrong_host"

# Enrol for real, with a genuine GitHub remote — that is what the owner and repository are read
# from, and a fixture that faked it would be testing something else.
repository_id="$(cli repository add "$fixture_root/repository" \
  --remote "https://github.com/owner/project.git" \
  --mode immediate --development-target development \
  --integration pull-request | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "enrollment did not return a repository ID"

# --- The token is opt-in, stored by the CLI, and never printed back. ---
status_without="$(plain github status "$repository_id")"
grep -q "No GitHub token is stored" <<<"$status_without" || \
  fail "a repository with no token does not say so: $status_without"

stored="$(printf 'test-token' | plain github set-token "$repository_id")"
grep -q "never written to logs" <<<"$stored" || \
  fail "storing a token does not say where it does and does not go: $stored"
grep -q "test-token" <<<"$stored" && fail "storing a token printed the token back"

token_file="$fixture_root/state/credentials/github-$repository_id.token"
[[ -f "$token_file" ]] || fail "no token file was written"
mode="$(stat -f '%Lp' "$token_file" 2>/dev/null || stat -c '%a' "$token_file")"
[[ "$mode" == "600" ]] || fail "the stored token is readable by others: mode $mode"

status_with="$(plain github status "$repository_id")"
grep -q "A GitHub token is stored" <<<"$status_with" || \
  fail "a stored token is not reported: $status_with"
grep -q "test-token" <<<"$status_with" && fail "token status printed the token itself"

feature_id="feature_00000000-0000-4000-8000-000000001201"
task_id="task_00000000-0000-4000-8000-000000001202"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Reach the target through a pull request",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"Add the reviewed change","dependencies":{},"acceptance_checks":[{"id":"present","description":"the change is ready"}]}
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
printf 'reviewed before it lands\n' >"$workspace_path/reviewed.txt"
package_id="$(cli package capture "$feature_id" --revision 1 --task "$task_id" | json_field package_id)"
unit_id="$(cli release create-unit "$feature_id" --revision 1 --task "$task_id" | json_field unit_id)"
cli schedule unit "$unit_id" --package-id "$package_id" --revision 1 >/dev/null
cli schedule release-now "$unit_id" >/dev/null

# --- The early publication is an ordinary push: the strategy changes the integration, not this. ---
development_head=''
for _ in {1..90}; do
  development_head="$(remote_ref refs/heads/development)"
  [[ -n "$development_head" ]] && break
  sleep 2
done
[[ -n "$development_head" ]] || \
  fail "the work never reached the development branch within 180 seconds"

# The integration is planned on its own, exactly as it is for a direct push — what differs is
# what happens when its time arrives. Brought forward here so the scenario does not wait out a
# time drawn at random from the policy window.
integration_slot=''
for _ in {1..60}; do
  if integration_slot="$(cli schedule show "$unit_id" 2>/dev/null)"; then
    break
  fi
  integration_slot=''
  sleep 2
done
[[ -n "$integration_slot" ]] || \
  fail "no release time was selected for integrating the work into the target"
cli schedule release-now "$unit_id" >/dev/null

# --- And then a pull request is opened, by the service, with nobody asking. ---
opened=''
for _ in {1..90}; do
  if grep -q '"number": 1' <<<"$(github_pulls)"; then
    opened=yes
    break
  fi
  sleep 2
done
[[ -n "$opened" ]] || \
  fail "no pull request was opened within 180 seconds: $(github_pulls)"

pulls="$(github_pulls)"
grep -q '"ref": "development"' <<<"$pulls" || \
  fail "the pull request does not come from the development branch: $pulls"
grep -q '"ref": "main"' <<<"$pulls" || \
  fail "the pull request is not aimed at the target: $pulls"

# --- Nothing has landed, and nothing claims it has. ---
[[ "$(remote_ref refs/heads/main)" == "$main_before" ]] || \
  fail "the target moved while a pull request was still open"

queue_json="$(cli queue status)"
grep -q '"state":"awaiting_merge"' <<<"$queue_json" || \
  fail "a unit waiting on a pull request is not reported as awaiting a merge: $queue_json"
grep -q '"number":1' <<<"$queue_json" || \
  fail "the queue does not name the pull request: $queue_json"

queue_text="$(plain queue status)"
grep -q "awaiting your merge" <<<"$queue_text" || \
  fail "the human-readable queue does not say what it is waiting for: $queue_text"
grep -q "is waiting for you to merge it" <<<"$queue_text" || \
  fail "the queue does not say who is expected to act: $queue_text"

status_before_merge="$(cli feature status "$feature_id" --revision 1)"
grep -q '"status":"published"' <<<"$status_before_merge" && \
  fail "a task whose pull request is still open is reported as published"

# --- Sit through further passes. The product must not merge, now or ever. ---
sleep 70
[[ "$(remote_ref refs/heads/main)" == "$main_before" ]] || \
  fail "the target moved without anyone merging the pull request"
grep -q '"merged": false' <<<"$(github_pulls)" || \
  fail "the pull request was merged by something other than a person: $(github_pulls)"
[[ "$(github_pulls | grep -c '"number"')" == "1" ]] || \
  fail "a second pull request was opened for work that already had one: $(github_pulls)"

# --- A person merges. Only now is the work finished. ---
github_control "/control/merge/1" >/dev/null

published=''
for _ in {1..90}; do
  if grep -q '"status":"published"' <<<"$(cli feature status "$feature_id" --revision 1)"; then
    published=yes
    break
  fi
  sleep 2
done
[[ -n "$published" ]] || \
  fail "a merged pull request did not finish its task within 180 seconds"

merged_queue="$(plain queue status)"
grep -q "was merged" <<<"$merged_queue" || \
  fail "the queue does not report the merge: $merged_queue"

printf 'PASS work reaches the target through a pull request that the service opens and only a person merges\n'
