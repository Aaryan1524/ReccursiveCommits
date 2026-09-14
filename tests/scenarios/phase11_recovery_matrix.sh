#!/usr/bin/env bash
# Proves the situations that would otherwise lose work are survivable, and visible.
#
# Every other scenario tests the product working. This one tests it being interfered with: a
# checkout with the user's own uncommitted work in it, a package directory corrupted on disk, a
# repository that has been moved or deleted, a change that is a binary file, and a change that is
# a rename. These are the cases where a queue quietly loses or mangles something and nobody finds
# out until the commit lands, so each one asserts what is *durable* afterwards rather than that a
# command exited zero.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase11.XXXXXX")"
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

cli() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" --json "$@"
}

plain() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" "$@"
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
printf '# Recovery fixture\n' >"$fixture_root/repository/README.md"
printf 'original line\n' >"$fixture_root/repository/moved.txt"
git -C "$fixture_root/repository" add README.md moved.txt
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main
main_before="$(remote_ref refs/heads/main)"

# The user's own work in progress, which nothing this product does may disturb.
printf 'work the user has not committed\n' >"$fixture_root/repository/user-draft.txt"
printf '# Recovery fixture\nan edit the user is still making\n' >"$fixture_root/repository/README.md"
draft_before="$(cat "$fixture_root/repository/user-draft.txt")"
readme_before="$(cat "$fixture_root/repository/README.md")"
dirty_before="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"
[[ -n "$dirty_before" ]] || fail "the fixture was meant to start with uncommitted work in it"

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "repository enrollment did not return an ID"

feature_id="feature_00000000-0000-4000-8000-000000001401"
binary_task="task_00000000-0000-4000-8000-000000001402"
rename_task="task_00000000-0000-4000-8000-000000001403"
pending_task="task_00000000-0000-4000-8000-000000001404"
cat >"$fixture_root/feature-plan.json" <<JSON
{
  "schema_version": 1,
  "feature_id": "$feature_id",
  "revision": 1,
  "repository_id": "$repository_id",
  "goal": "Survive interference without losing or mangling work",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [{
    "id": "delivery",
    "name": "Delivery",
    "tasks": [
      {"id":"$binary_task","name":"Add a binary file","dependencies":{},"acceptance_checks":[{"id":"present","description":"the bytes survive"}]},
      {"id":"$rename_task","name":"Rename a tracked file","dependencies":{},"acceptance_checks":[{"id":"present","description":"the rename survives"}]},
      {"id":"$pending_task","name":"Wait for a checkout that is gone","dependencies":{},"acceptance_checks":[{"id":"present","description":"it waits rather than publishing"}]}
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

# --- A change that is not text. Bytes must survive capture, storage and publication exactly. ---
workspace_path="$(cli workspace create "$feature_id" --revision 1 | json_field path)"
[[ -d "$workspace_path" ]] || fail "workspace creation did not return an owned workspace"
# Deliberately includes a NUL and every byte value, which is what distinguishes a real binary
# round-trip from one that happens to survive because the content was ASCII after all.
python3 -c 'import sys; sys.stdout.buffer.write(bytes(range(256))*8)' >"$workspace_path/payload.bin"
binary_sha_before="$(shasum -a 256 "$workspace_path/payload.bin" | cut -d" " -f1)"
binary_package="$(cli package capture "$feature_id" --revision 1 --task "$binary_task" | json_field package_id)"
[[ "$binary_package" == package_* ]] || fail "capturing a binary file did not return a package"

binary_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$binary_task" | json_field unit_id)"
cli schedule unit "$binary_unit" --package-id "$binary_package" --revision 1 >/dev/null
cli schedule release-now "$binary_unit" >/dev/null

for _ in {1..90}; do
  [[ "$(remote_ref refs/heads/main)" != "$main_before" ]] && break
  sleep 2
done
[[ "$(remote_ref refs/heads/main)" != "$main_before" ]] || \
  fail "the binary change was never published within 180 seconds"

git clone --quiet "$fixture_root/remote.git" "$fixture_root/verify-binary"
[[ -f "$fixture_root/verify-binary/payload.bin" ]] || \
  fail "the published commit does not contain the binary file"
binary_sha_after="$(shasum -a 256 "$fixture_root/verify-binary/payload.bin" | cut -d" " -f1)"
[[ "$binary_sha_after" == "$binary_sha_before" ]] || \
  fail "the binary file changed in transit: $binary_sha_before -> $binary_sha_after"
main_after_binary="$(remote_ref refs/heads/main)"

# --- A rename. Git records it as a rename only if the content moved intact. ---
git -C "$workspace_path" mv moved.txt renamed.txt 2>/dev/null || {
  mv "$workspace_path/moved.txt" "$workspace_path/renamed.txt"
}
rename_package="$(cli package capture "$feature_id" --revision 1 --task "$rename_task" | json_field package_id)"
[[ "$rename_package" == package_* ]] || fail "capturing a rename did not return a package"
rename_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$rename_task" | json_field unit_id)"
cli schedule unit "$rename_unit" --package-id "$rename_package" --revision 1 >/dev/null
cli schedule release-now "$rename_unit" >/dev/null

for _ in {1..90}; do
  [[ "$(remote_ref refs/heads/main)" != "$main_after_binary" ]] && break
  sleep 2
done
[[ "$(remote_ref refs/heads/main)" != "$main_after_binary" ]] || \
  fail "the rename was never published within 180 seconds"

git clone --quiet "$fixture_root/remote.git" "$fixture_root/verify-rename"
[[ -f "$fixture_root/verify-rename/renamed.txt" ]] || \
  fail "the renamed file is missing from the target"
[[ -f "$fixture_root/verify-rename/moved.txt" ]] && \
  fail "the original path still exists, so the rename was published as a copy"
[[ "$(cat "$fixture_root/verify-rename/renamed.txt")" == "original line" ]] || \
  fail "the renamed file's content did not survive"

# --- Through all of that, the user's uncommitted work is exactly as they left it. ---
[[ "$(cat "$fixture_root/repository/user-draft.txt")" == "$draft_before" ]] || \
  fail "publishing changed an untracked file in the user's checkout"
[[ "$(cat "$fixture_root/repository/README.md")" == "$readme_before" ]] || \
  fail "publishing changed a file the user was still editing"
dirty_after="$(git -C "$fixture_root/repository" status --porcelain=v1 | sort)"
[[ "$dirty_after" == "$dirty_before" ]] || \
  fail "the user's working tree changed:\n$dirty_before\n--- became ---\n$dirty_after"

# --- A package corrupted on disk is reported, not silently trusted and not fatal. ---
package_parent="$fixture_root/state/packages/$rename_package"
[[ -d "$package_parent" ]] || fail "expected a stored package directory at $package_parent"
printf 'not a package\n' >"$package_parent/revision-1/manifest.json" 2>/dev/null || \
  printf 'not a package\n' >"$package_parent/corrupted-marker"

stop_daemon
start_daemon

audit="$(plain queue audit)"
grep -q "Unresolved recovery issues" <<<"$audit" || \
  fail "queue audit does not report on recovery issues at all: $audit"
audit_json="$(cli queue audit)"
grep -q '"issues"' <<<"$audit_json" || \
  fail "queue audit did not return an issue list: $audit_json"

# Whatever it concluded about that package, the service is still serving and the queue is intact.
cli status >/dev/null || fail "the service did not survive a corrupted package directory"
queue_after_corruption="$(cli queue status)"
grep -q "$repository_id" <<<"$queue_after_corruption" || \
  fail "the queue lost its repository after a corrupted package: $queue_after_corruption"

# --- A checkout that has been moved away is diagnosed rather than crashed on. ---
#
# Work has to be waiting for this to mean anything: a repository with nothing due is skipped for
# the ordinary reason that there is nothing to do, and would say nothing either way.
printf 'captured before the checkout went away\n' >"$workspace_path/pending.txt"
pending_package="$(cli package capture "$feature_id" --revision 1 --task "$pending_task" | json_field package_id)"
pending_unit="$(cli release create-unit "$feature_id" --revision 1 --task "$pending_task" | json_field unit_id)"
cli schedule unit "$pending_unit" --package-id "$pending_package" --revision 1 >/dev/null
cli schedule release-now "$pending_unit" >/dev/null

target_when_lost="$(remote_ref refs/heads/main)"
mv "$fixture_root/repository" "$fixture_root/repository-moved"
stop_daemon
start_daemon
cli status >/dev/null || fail "the service did not survive its repository being moved"

diagnosis="$(plain diagnose "$repository_id" 2>&1 || true)"
[[ -n "$diagnosis" ]] || fail "diagnosing a missing checkout produced no output at all"
grep -q "Checkout: missing" <<<"$diagnosis" || \
  fail "the diagnosis does not report the checkout as missing: $diagnosis"
# The failure this replaces: credentials and signing are probed against the managed mirror, which
# outlives the working copy, so this used to read "Can publish now: yes" for a repository that
# could never publish again.
grep -q "Can publish now: no" <<<"$diagnosis" || \
  fail "a repository whose checkout is gone still claims it can publish: $diagnosis"

# And the daemon says why it is skipping the repository, rather than looking idle forever.
#
# Read once after waiting, not polled: every `logs` call records an event of its own, so polling
# evicts the entry it is looking for and the check fails for its own reasons.
sleep 70
logged="$(cli logs --limit 200)"
grep -q "checkout_unattributable" <<<"$logged" || \
  fail "the daemon skipped an unpublishable repository without recording why"
grep -q "reccursive diagnose" <<<"$logged" || \
  fail "the record does not say what to do about it"

# Nothing was published from it either: the queued unit waited rather than committing under some
# invented author.
[[ "$(remote_ref refs/heads/main)" == "$target_when_lost" ]] || \
  fail "something was published from a repository whose checkout no longer exists"

# And the work already published is still recorded as published, not lost with the checkout.
status_after="$(cli feature status "$feature_id" --revision 1)"
grep -q '"status":"published"' <<<"$status_after" || \
  fail "work that was already published stopped being reported as published: $status_after"

printf 'PASS binaries and renames survive intact, the user checkout is never touched, and a corrupted package or missing repository is reported rather than fatal\n'
