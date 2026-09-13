#!/usr/bin/env bash
# Proves an out-of-scope submission fails *without modifying the queue*.
#
# The second half is the part that matters and the part nothing else covers. A refusal that still
# leaves a package on disk, a task advanced, or a unit created has not protected anything — it has
# just reported an error over a queue it already changed. So every attempt below is followed by the
# same question: is the queue exactly as it was?
#
# Every attempt goes through the real CLI. Library tests already prove the individual validators
# reject these inputs; what they cannot prove is that the validator is on the path a caller reaches.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase6c.XXXXXX")"
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

# The queue's whole observable state: packages on disk, packages the store knows about, and every
# task's durable status. Compared before and after each refusal.
queue_fingerprint() {
  ls -1 "$fixture_root/state/packages" 2>/dev/null | sort
  printf -- '--\n'
  cli queue audit 2>/dev/null | grep -o '"package_id":"[^"]*"' | sort
  printf -- '--\n'
  cli feature status "$feature_id" 2>/dev/null | grep -o '"status":"[^"]*"' | sort
}

# Runs a command that must fail, and proves the queue did not move.
refuses_without_changing_the_queue() {
  local description="$1"
  shift
  local before after
  before="$(queue_fingerprint)"
  if cli "$@" >"$fixture_root/refusal.json" 2>&1; then
    fail "$description was accepted: $(cat "$fixture_root/refusal.json")"
  fi
  after="$(queue_fingerprint)"
  if [[ "$before" != "$after" ]]; then
    printf 'before:\n%s\nafter:\n%s\n' "$before" "$after" >&2
    fail "$description changed the queue while being refused"
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
printf '# Boundary fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

# A second repository that is never enrolled, and a secret outside every repository.
git init --quiet --initial-branch=main "$fixture_root/unenrolled"
printf 'SECRET=hunter2\n' >"$fixture_root/outside-secret.txt"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

feature_id="feature_00000000-0000-4000-8000-000000000c01"
task_id="task_00000000-0000-4000-8000-000000000c02"
foreign_task="task_00000000-0000-4000-8000-000000000cff"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Stay inside the boundary",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$task_id","name":"In scope","dependencies":{},"acceptance_checks":[{"id":"c","description":"present"}]}
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
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
JSON
cli schedule set-policy "$repository_id" "$fixture_root/schedule-policy.json" >/dev/null

# ---- The workspace is the only place an agent may write, and it chooses none of its contents ----

refuses_without_changing_the_queue "an absolute prerequisite path" \
  workspace create "$feature_id" --revision 1 --include "$fixture_root/outside-secret.txt"
refuses_without_changing_the_queue "a parent-directory prerequisite path" \
  workspace create "$feature_id" --revision 1 --include "../outside-secret.txt"
refuses_without_changing_the_queue "a prerequisite path escaping through a subdirectory" \
  workspace create "$feature_id" --revision 1 --include "src/../../outside-secret.txt"

# ---- A plan may only describe work in a repository the user enrolled, on the branch they chose ----

unenrolled_id="repo_00000000-0000-4000-8000-0000000000ff"
sed "s/$repository_id/$unenrolled_id/" "$fixture_root/feature-plan.json" \
  >"$fixture_root/unenrolled-plan.json"
refuses_without_changing_the_queue "a plan naming an unenrolled repository" \
  plan import "$fixture_root/unenrolled-plan.json"

sed -e 's|"refs/heads/main"|"refs/heads/attacker"|' \
    -e "s/\"feature_id\": \"$feature_id\"/\"feature_id\": \"feature_00000000-0000-4000-8000-000000000c09\"/" \
    "$fixture_root/feature-plan.json" >"$fixture_root/redirected-plan.json"
refuses_without_changing_the_queue "a plan redirecting publication to another branch" \
  plan import "$fixture_root/redirected-plan.json"

# ---- A submission may only name work that the authorized plan revision actually contains ----

refuses_without_changing_the_queue "a submission naming a task outside the plan revision" \
  task submit "$feature_id" --revision 1 --task "$foreign_task"
refuses_without_changing_the_queue "a submission against a plan revision that does not exist" \
  task submit "$feature_id" --revision 9 --task "$task_id"
refuses_without_changing_the_queue "a submission for a feature that was never imported" \
  task submit "feature_00000000-0000-4000-8000-000000000cee" --task "$task_id"

# ---- Content the user would not want published is refused at capture, leaving nothing behind ----

workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
printf 'API_KEY=AKIAIOSFODNN7EXAMPLE\n' >"$workspace_path/.env"
refuses_without_changing_the_queue "a capture carrying a blocked .env file" \
  task submit "$feature_id" --revision 1 --task "$task_id"
rm "$workspace_path/.env"

# The refusal must name the rule and the path, and must not echo the secret it found.
printf 'AKIAIOSFODNN7EXAMPLE\n' >"$workspace_path/credentials.txt"
cli task submit "$feature_id" --revision 1 --task "$task_id" \
  >"$fixture_root/secret-refusal.json" 2>&1 && fail "a capture carrying a secret was accepted"
grep -q 'credentials.txt' "$fixture_root/secret-refusal.json" || \
  fail "the refusal did not name the offending path: $(cat "$fixture_root/secret-refusal.json")"
grep -q 'AKIAIOSFODNN7EXAMPLE' "$fixture_root/secret-refusal.json" && \
  fail "the refusal echoed the secret it found"
rm "$workspace_path/credentials.txt"

# ---- After every refusal, in-scope work still succeeds: the boundary rejects, it does not wedge ----

printf 'legitimate work\n' >"$workspace_path/allowed.txt"
accepted="$(cli task submit "$feature_id" --revision 1 --task "$task_id")"
grep -q '"created":true' <<<"$accepted" || \
  fail "in-scope work was not accepted after the refusals: $accepted"
[[ "$(ls -1 "$fixture_root/state/packages" | wc -l | tr -d ' ')" == "1" ]] || \
  fail "the refused attempts left packages behind"

# ---- What verifies a change is never something the change supplied ----

# There is no command to add, alter, enable, or disable a trusted check: they are registered by the
# daemon when a repository is enrolled and are not part of any client request. A caller that tries
# to name its own is a usage error, not a stricter release.
"$binary_dir/reccursive" --state-dir "$fixture_root/state" --json \
  task submit "$feature_id" --revision 1 --task "$task_id" --check my-own-check \
  >"$fixture_root/check-attempt.json" 2>&1 && fail "a client-named release check was accepted"
grep -qi 'unexpected argument' "$fixture_root/check-attempt.json" || \
  fail "naming a release check failed for the wrong reason: $(cat "$fixture_root/check-attempt.json")"

printf 'PASS every out-of-scope attempt was refused with the queue unchanged, and in-scope work still succeeded\n'
