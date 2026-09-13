#!/usr/bin/env bash
# Proves a plan can be prepared, reviewed, and revised in the terminal without disturbing work that
# has already been published.
#
# The editor step is exercised for real: EDITOR is set to a script that makes a scripted change, so
# the whole edit path runs end to end rather than around a test double inside the code.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary_dir="${RECCURSIVE_BIN_DIR:-$project_root/target/debug}"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-phase8b.XXXXXX")"
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

plain() {
  "$binary_dir/reccursive" --state-dir "$fixture_root/state" "$@"
}

revision_count() {
  cli plan history "$feature_id" 2>/dev/null | grep -o '"revision":' | wc -l | tr -d ' '
}

cd "$project_root"
if [[ ! -x "$binary_dir/reccursive" || ! -x "$binary_dir/reccursive-daemon" ]]; then
  cargo build --workspace --locked
fi

git init --quiet --bare --initial-branch=main "$fixture_root/remote.git"
git init --quiet --initial-branch=main "$fixture_root/repository"
git -C "$fixture_root/repository" config user.name "Scenario Fixture"
git -C "$fixture_root/repository" config user.email "fixture@example.invalid"
printf '# Plan authoring fixture\n' >"$fixture_root/repository/README.md"
git -C "$fixture_root/repository" add README.md
git -C "$fixture_root/repository" commit --quiet -m "Initialize fixture"
git -C "$fixture_root/repository" remote add origin "$fixture_root/remote.git"
git -C "$fixture_root/repository" push --quiet -u origin main

start_daemon
repository_id="$(cli repository add "$fixture_root/repository" | json_field id)"

# ---- A template is a plan that already imports ----

plain plan template "$repository_id" --target main -o "$fixture_root/template.json" >/dev/null
template_check="$(plain plan check "$fixture_root/template.json")"
grep -q "is a valid plan" <<<"$template_check" || \
  fail "the template did not pass its own validation"
feature_ids="$(grep -o '"feature_id": "[^"]*"' "$fixture_root/template.json")"
feature_id="$(sed -E 's/.*"feature_id": "([^"]*)".*/\1/' <<<"${feature_ids%%$'\n'*}")"
[[ "$feature_id" == feature_* ]] || fail "the template has no feature id"
cli plan import "$fixture_root/template.json" >/dev/null || fail "the template did not import"
[[ "$(revision_count)" == "1" ]] || fail "importing the template did not produce exactly one revision"

# ---- Validation happens locally, and a rejected document never reaches the daemon ----

refuses_locally() {
  local description="$1" expected="$2" file="$3"
  local before after
  before="$(revision_count)"
  if plain plan check "$file" >"$fixture_root/check.txt" 2>&1; then
    fail "$description was accepted: $(cat "$fixture_root/check.txt")"
  fi
  grep -qi "$expected" "$fixture_root/check.txt" || \
    fail "$description was not reported by name ($expected): $(cat "$fixture_root/check.txt")"
  after="$(revision_count)"
  [[ "$before" == "$after" ]] || fail "$description changed stored revisions while being rejected"
}

python3 - "$fixture_root/template.json" "$fixture_root/no-checks.json" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1]))
plan["phases"][0]["tasks"][0]["acceptance_checks"] = []
json.dump(plan, open(sys.argv[2], "w"))
PY
refuses_locally "a task with no acceptance check" "acceptance check" "$fixture_root/no-checks.json"

python3 - "$fixture_root/template.json" "$fixture_root/duplicate-phase.json" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1]))
plan["phases"].append(dict(plan["phases"][0]))
json.dump(plan, open(sys.argv[2], "w"))
PY
refuses_locally "a repeated phase" "more than once" "$fixture_root/duplicate-phase.json"

python3 - "$fixture_root/template.json" "$fixture_root/cycle.json" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1]))
tasks = plan["phases"][0]["tasks"]
# Each waits for the other, which no ordering can satisfy.
tasks[0]["dependencies"] = {tasks[1]["id"]: "target_published"}
tasks[1]["dependencies"] = {tasks[0]["id"]: "target_published"}
json.dump(plan, open(sys.argv[2], "w"))
PY
refuses_locally "a dependency cycle" "cycle" "$fixture_root/cycle.json"

# ---- Review shows the work itself, not a count of it ----

review="$(plain plan show "$feature_id")"
grep -q "First unit of work" <<<"$review" || fail "the review does not list task names: $review"
grep -q "waits for" <<<"$review" || fail "the review does not show dependencies: $review"
grep -q "done when:" <<<"$review" || fail "the review does not show acceptance checks: $review"
grep -q "draft" <<<"$review" || fail "the review does not say the plan is still a draft: $review"

# ---- Export reproduces a stored revision, and re-importing it is refused ----

plain plan export "$feature_id" --revision 1 -o "$fixture_root/exported.json" >/dev/null
exported_check="$(plain plan check "$fixture_root/exported.json")"
grep -q "is a valid plan" <<<"$exported_check" || \
  fail "an exported revision does not pass validation"
before_reimport="$(revision_count)"
cli plan import "$fixture_root/exported.json" >"$fixture_root/reimport.json" 2>&1 && \
  fail "re-importing an unchanged revision was accepted as a new one"
[[ "$(revision_count)" == "$before_reimport" ]] || \
  fail "a refused re-import still changed stored revisions"

# ---- Publish one task, then edit the plan. The published work must not move. ----

cli plan seal "$feature_id" >/dev/null
sealed_revision=2
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
workspace_path="$(cli workspace create "$feature_id" --revision "$sealed_revision" | json_field path)"
sealed_plan="$(cli plan show "$feature_id" --revision "$sealed_revision")"
task_ids="$(grep -o '"id":"task_[^"]*"' <<<"$sealed_plan")"
first_task="$(sed -E 's/.*"(task_[^"]*)".*/\1/' <<<"${task_ids%%$'\n'*}")"
[[ "$first_task" == task_* ]] || fail "could not read the first task id"
printf 'first unit\n' >"$workspace_path/first.txt"
cli task submit "$feature_id" --revision "$sealed_revision" --task "$first_task" >/dev/null

status_before="$(cli feature status "$feature_id" --revision "$sealed_revision")"
plan_before="$(cli plan show "$feature_id" --revision "$sealed_revision")"

# The editor is a real program doing a real edit. Only the goal changes, so anything that moves in
# the published revision moved because editing disturbed it.
cat >"$fixture_root/editor.sh" <<'EDITOR'
#!/usr/bin/env bash
python3 - "$1" <<'PY'
import json, sys
path = sys.argv[1]
plan = json.load(open(path))
plan["goal"] = "A revised goal that must not touch what already shipped"
json.dump(plan, open(path, "w"), indent=2)
PY
EDITOR
chmod +x "$fixture_root/editor.sh"

EDITOR="$fixture_root/editor.sh" cli plan edit "$feature_id" >"$fixture_root/edited.json" 2>&1 || \
  fail "editing the plan failed: $(cat "$fixture_root/edited.json")"
grep -q '"revision":3' "$fixture_root/edited.json" || \
  fail "the edit did not append revision 3: $(cat "$fixture_root/edited.json")"

# This is the exit criterion. The revision that was published is byte-identical, and so is the
# durable state of the task published from it.
[[ "$(cli plan show "$feature_id" --revision "$sealed_revision")" == "$plan_before" ]] || \
  fail "editing altered the already-published plan revision"
[[ "$(cli feature status "$feature_id" --revision "$sealed_revision")" == "$status_before" ]] || \
  fail "editing altered the durable state of already-published work"

# The appended revision is a draft: an edit changes scope, so it cannot inherit the seal.
appended_json="$(cli plan show "$feature_id" --revision 3)"
grep -q '"sealed":false' <<<"$appended_json" || \
  fail "the edited revision inherited the seal from the revision it was derived from"
appended_review="$(plain plan show "$feature_id" --revision 3)"
grep -q "A revised goal" <<<"$appended_review" || \
  fail "the edit was not applied to the appended revision"

# ---- An editor that changes nothing appends nothing ----

cat >"$fixture_root/noop-editor.sh" <<'EDITOR'
#!/usr/bin/env bash
exit 0
EDITOR
chmod +x "$fixture_root/noop-editor.sh"
before_noop="$(revision_count)"
EDITOR="$fixture_root/noop-editor.sh" cli plan edit "$feature_id" \
  >"$fixture_root/noop.json" 2>&1 && fail "an edit that changed nothing appended a revision"
[[ "$(revision_count)" == "$before_noop" ]] || fail "an unchanged edit still appended a revision"

# ---- An editor that produces an invalid plan imports nothing ----

cat >"$fixture_root/breaking-editor.sh" <<'EDITOR'
#!/usr/bin/env bash
python3 - "$1" <<'PY'
import json, sys
plan = json.load(open(sys.argv[1]))
plan["phases"][0]["tasks"][0]["acceptance_checks"] = []
json.dump(plan, open(sys.argv[1], "w"))
PY
EDITOR
chmod +x "$fixture_root/breaking-editor.sh"
before_broken="$(revision_count)"
EDITOR="$fixture_root/breaking-editor.sh" cli plan edit "$feature_id" \
  >"$fixture_root/broken.json" 2>&1 && fail "an edit producing an invalid plan was imported"
grep -q "acceptance check" "$fixture_root/broken.json" || \
  fail "the invalid edit was not reported by name: $(cat "$fixture_root/broken.json")"
[[ "$(revision_count)" == "$before_broken" ]] || \
  fail "an invalid edit still appended a revision"

printf 'PASS a plan was templated, validated, reviewed, exported, and revised with published work untouched\n'
