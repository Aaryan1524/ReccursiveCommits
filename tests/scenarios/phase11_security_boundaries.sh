#!/usr/bin/env bash
# Proves the boundaries a secret or an attacker would have to cross, from outside the code.
#
# Unit tests can show a function refuses bad input. What they cannot show is that the assembled
# product refuses it too — that the socket really is owner-only on disk, that a wrong token really
# is rejected by the running service, that an exported diagnostic bundle really does not contain
# the credentials sitting next to it. Each of those is asserted here against a live daemon.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase11-sec.XXXXXX")"
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

mode_of() {
  stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Security fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"
[[ "$repository_id" == repo_* ]] || fail "enrollment did not return a repository ID"

# --- Nothing another user on this machine can read. ---
for guarded in state/service.sock state/auth.token state/state.sqlite; do
  path="$fixture_root/$guarded"
  [[ -e "$path" ]] || continue
  mode="$(mode_of "$path")"
  [[ "${mode: -2}" == "00" ]] || \
    fail "$guarded is readable or writable by others: mode $mode"
done
state_mode="$(mode_of "$fixture_root/state")"
[[ "${state_mode: -2}" == "00" ]] || \
  fail "the state directory is accessible to others: mode $state_mode"

# --- The socket is not an open door: the token actually gates it. ---
#
# Reaching the socket is not the same as being allowed to use it. A second state directory whose
# socket points at the live one but whose token is wrong is exactly the position an attacker who
# found the socket would be in.
[[ -S "$fixture_root/state/service.sock" ]] || fail "expected a Unix socket in the state directory"
real_token="$(cat "$fixture_root/state/auth.token")"
[[ ${#real_token} -ge 32 ]] || fail "the local API token is too short to be a credential"

mkdir -p "$fixture_root/impostor"
ln -s "$fixture_root/state/service.sock" "$fixture_root/impostor/service.sock"
printf 'w%.0s' $(seq 1 40) >"$fixture_root/impostor/auth.token"
if rejected="$("$binary_dir/reccursive" --state-dir "$fixture_root/impostor" --json status 2>&1)"; then
  fail "the service answered a caller holding the wrong token: $rejected"
fi
grep -qiE "unauthor|authentication" <<<"$rejected" || \
  fail "a wrong token was refused for some other reason than authentication: $rejected"
grep -qF "$real_token" <<<"$rejected" && \
  fail "the refusal handed the real token to the caller that guessed wrong"

# The real token still works, so the check above is refusing the token rather than the connection.
cli status >/dev/null || fail "the correct token stopped working"

# --- A GitHub token identifier cannot escape the credentials directory. ---
if escaped="$(printf 'sneaky' | plain github set-token '../../escaped' 2>&1)"; then
  fail "a repository identifier that leaves the credentials directory was accepted"
fi
grep -qiE "invalid|not a|identifier|unusable" <<<"$escaped" || \
  fail "the refusal does not say what was wrong with the identifier: $escaped"
[[ -z "$(find "$fixture_root" -name '*escaped*' -print -quit)" ]] || \
  fail "a refused identifier still wrote a file somewhere"

# --- A stored token is owner-only, and no command prints it back. ---
printf 'ghp_unmistakable_secret_value' | plain github set-token "$repository_id" >/dev/null
token_file="$fixture_root/state/credentials/github-$repository_id.token"
[[ -f "$token_file" ]] || fail "no token file was written"
token_mode="$(mode_of "$token_file")"
[[ "$token_mode" == "600" ]] || fail "the stored token is readable by others: mode $token_mode"
credentials_mode="$(mode_of "$fixture_root/state/credentials")"
[[ "$credentials_mode" == "700" ]] || \
  fail "the credentials directory is accessible to others: mode $credentials_mode"

for command in "github status $repository_id" "repository list" "queue status" "logs --limit 100" \
               "diagnose $repository_id"; do
  # shellcheck disable=SC2086
  output="$(plain $command 2>&1 || true)"
  grep -q "ghp_unmistakable_secret_value" <<<"$output" && \
    fail "'$command' printed the stored GitHub token"
done

# --- The diagnostic export is safe to send someone. ---
plain diagnostics export "$fixture_root/export" >/dev/null
[[ -n "$(ls -A "$fixture_root/export")" ]] || fail "the diagnostics export wrote nothing"
if grep -rq "ghp_unmistakable_secret_value" "$fixture_root/export"; then
  fail "the diagnostics export contains the GitHub token"
fi
if grep -rqF "$real_token" "$fixture_root/export"; then
  fail "the diagnostics export contains the local API token"
fi
[[ -z "$(find "$fixture_root/export" -name '*.sqlite*' -print -quit)" ]] || \
  fail "the diagnostics export contains the queue database"
[[ -z "$(find "$fixture_root/export" -name '*.token' -print -quit)" ]] || \
  fail "the diagnostics export contains a credential file"

# --- Secrets written into events are masked rather than stored verbatim. ---
# Enrolling a remote with embedded credentials is refused outright; this checks the other half,
# that a credential reaching an event is masked on the way in.
if leaked="$(cli repository add "$fixture_root/repository" \
  --remote "https://user:hunter2@example.invalid/p.git" 2>&1)"; then
  fail "a remote carrying embedded credentials was accepted"
fi
grep -q "hunter2" <<<"$(plain logs --limit 100)" && \
  fail "a rejected credential was written into the event log in the clear"

printf 'PASS the socket, the database and both credentials are owner-only, identifiers cannot escape, and nothing prints or exports a secret\n'
