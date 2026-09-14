# Scenario scripts

Each script proves one phase's claim end to end, against the real CLI and a real
daemon over a real socket. They exist because this codebase's recurring defect is
tested library code that nothing calls: a unit test can pass while the code path
a user reaches is broken or absent.

Every script must be registered **by name** in `.github/workflows/ci.yml`. An
unregistered script silently never runs.

## Never pipe into `grep -q` (or `head`)

All of these run under `set -euo pipefail`. A quiet grep exits the instant it
matches, which closes the pipe; the command on the left then dies of SIGPIPE and
exits 141, and `pipefail` reports that as the pipeline's status. The assertion
fails *because it found what it was looking for*, and only when the match is near
the start of the output — so it passes locally, passes on Linux, and fails on
macOS, or passes for a year and then fails when output order changes.

```sh
# Wrong: fails when the match is on an early line.
git log --format=%s | grep -Fxq "$subject" || fail "..."

# Right: read once, match from a variable.
subjects="$(git log --format=%s)"
grep -Fxq "$subject" <<<"$subjects" || fail "..."
```

This has bitten the suite twice: once as a macOS-only CI failure across three
stacked PRs, and again the same day in a newly written scenario. `head -1` at the
end of a pipeline has the same hazard for the same reason.

## Leave nothing behind

A scenario creates its fixtures under `mktemp -d` and removes them in an `EXIT`
trap, including the daemon it started. Nothing may touch a real repository, the
user's checkout, or `~/Library/LaunchAgents`.

## Assert what is durable

Prefer asserting the state the daemon actually stored — `feature status`,
`release attempts`, the remote's own history — over the wording of a message.
Where a refusal is the point, assert that the queue did **not** change, not only
that the command failed: a refusal that half-completes is the defect.
