# Command-line interface

The Phase 1 CLI talks only to the authenticated local daemon. It never opens the
SQLite database or managed repositories directly.

Start the daemon in a separate terminal during development:

```sh
cargo run -p reccursive-daemon -- --state-dir /path/to/state
```

Use the same state directory for client commands:

```sh
cargo run -p reccursive-cli -- --state-dir /path/to/state doctor
cargo run -p reccursive-cli -- --state-dir /path/to/state repository add /path/to/repo
cargo run -p reccursive-cli -- --state-dir /path/to/state repository list
cargo run -p reccursive-cli -- --state-dir /path/to/state status
cargo run -p reccursive-cli -- --state-dir /path/to/state logs --limit 50
cargo run -p reccursive-cli -- --state-dir /path/to/state plan import feature-plan.json
cargo run -p reccursive-cli -- --state-dir /path/to/state plan seal feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state plan show feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state plan history feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace create feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace create feature_<uuid> --include src/config.rs
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace show feature_<uuid> --revision 1
cargo run -p reccursive-cli -- --state-dir /path/to/state package capture feature_<uuid> --revision 1 --task task_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state package show package_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state task submit feature_<uuid> --revision 1 --task task_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state task cancel feature_<uuid> --revision 1 --task task_<uuid> --message "replaced by a newer approach"
cargo run -p reccursive-cli -- --state-dir /path/to/state feature status feature_<uuid> --revision 1
cargo run -p reccursive-cli -- --state-dir /path/to/state release publish package_<uuid> --revision 1 --message "feat: publish the unit" --author-name "Your Name" --author-email you@example.invalid
cargo run -p reccursive-cli -- --state-dir /path/to/state release attempt attempt_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state release attempts --package-id package_<uuid> --limit 50
cargo run -p reccursive-cli -- --state-dir /path/to/state release create-unit feature_<uuid> --revision 1 --task task_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state release show-unit unit_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule set-policy repo_<uuid> schedule-policy.json
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule show-policy repo_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule unit unit_<uuid> --package-id package_<uuid> --revision 1
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule show unit_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule withdraw unit_<uuid> --reason "policy under review"
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule recalculate repo_<uuid> --reason "policy revised"
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule catch-up repo_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule set-override repo_<uuid> schedule-override.json
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule due --concurrency-limit 10
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule preview repo_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule pause repo_<uuid> --reason "investigating a failure"
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule resume repo_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state schedule release-now unit_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state diagnose repo_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state integrations
cargo run -p reccursive-cli -- --state-dir /path/to/state service install
cargo run -p reccursive-cli -- --state-dir /path/to/state service status
cargo run -p reccursive-cli -- --state-dir /path/to/state service show
cargo run -p reccursive-cli -- --state-dir /path/to/state service uninstall
cargo run -p reccursive-cli -- --state-dir /path/to/state queue audit
cargo run -p reccursive-cli -- --state-dir /path/to/state queue export /absolute/path/to/queue-backup
```

Pass `--json` for a stable JSON envelope with no prompts or spinners. Successful
responses are written to standard output; failures are written to standard error
with the same `{ "ok": false, "error": { "code": "...", "message": "..." } }` shape,
including command and argument mistakes. `--help` and `--version` are successful
commands and write their human-readable output to standard output. The
`RECCURSIVE_STATE_DIR` environment variable can replace `--state-dir`.

`doctor --json` intentionally writes its complete multi-check report to standard
output even when it finds a problem, so callers always receive one report rather
than an incomplete result plus a separate error object.

Run `reccursive --help` for a short start-here path, or use
`reccursive <command> --help` to inspect a command group before making changes.

An agent driving the CLI should start from
[`AGENT_HANDOFF.md`](AGENT_HANDOFF.md), which specifies the whole sequence.

## Repeating a request safely

`--idempotency-key KEY` names what an invocation is *for*, so repeating it returns
the first result instead of acting again:

```sh
reccursive --idempotency-key "agent-7/capture/task_<uuid>" \
  package capture feature_<uuid> --revision 1 --task task_<uuid>
```

This exists for callers that retry — an agent whose connection drops cannot tell a
request that never arrived from one that arrived, acted, and answered. A person
typing a command has no dropped connection to recover from and can leave the flag
alone.

The key is claimed before the command runs, not after it succeeds, because the
window in which a retry is dangerous is exactly the window in which the first
attempt is still running. Within that window a second request carrying the same key
is told the original is still running and to retry, rather than being allowed to act
alongside it. Afterwards it is handed the original outcome, success or failure,
unchanged.

Reusing one key for a genuinely different request is a conflict, not a cache hit:
the daemon compares a digest of the request and refuses rather than answering with
something the caller did not ask for. Keys on read-only commands are accepted and
ignored, since repeating a query is already safe and recording its answer would
freeze state that has since moved.

A failure that provably never left the machine gives the key back, so a real retry
runs. A failure anywhere a push could already have reached the remote keeps it and
replays the recorded failure: the error alone does not say whether the remote moved,
and `release attempt` is where that is settled.

`logs` returns the newest structured daemon events first. Every authenticated API
request is correlated by request ID, stored with a stable event kind and severity,
and scrubbed for common credential formats before persistence. The installation
retains a bounded event history rather than growing the database indefinitely.

`plan import` accepts the portable, versioned JSON contract documented in
[`PLAN_FORMAT.md`](PLAN_FORMAT.md). Imports are append-only: the first revision is
1 and each later document must use the next revision for the same feature and
repository. `plan show` reads an exact revision with `--revision` or the newest
revision by default; `plan history` lists every preserved revision.

`workspace create` checks out the plan's exact target commit into daemon-owned
storage. It never switches the user's branch or writes to the user's working tree
or index. A dirty file is included only when its repository-relative path is named
with `--include`; unmentioned changes remain solely in the user's checkout.
Renames, copies, unresolved merges, unsafe paths, clean paths presented as dirty
prerequisites, and duplicate ownership are rejected rather than guessed.

`package capture` snapshots the complete owned workspace for the selected plan
tasks. The immutable package contains a self-contained Git object bundle plus an
authenticated manifest with exact base/result trees. `package show` recalculates
the bundle and manifest hashes before returning metadata; corruption is an error.
Before the package is created, capture validates the exact prospective Git tree.
The current default policy blocks `.env` paths, generated output under
`node_modules/`, `target/`, `dist/`, `build/`, and `coverage/`, source maps and
minified JavaScript, files larger than 10 MiB, and several high-confidence private
key, GitHub, AWS, and API-key patterns. A failure identifies the rule and the
repository-relative path only; it never returns matched secret content.

Capture also runs daemon-owned trusted checks in the isolated workspace. New
repositories receive `git diff --check {base_commit}` with a 30-second limit;
the command and its result are stored separately from agent output and package
metadata. A failed or timed-out check blocks capture after recording evidence.

`task submit` is the whole submission in one step: it captures the task's work,
groups it into a release unit, and selects its durable release time. Every step is
one the caller could take separately; what submitting adds is that *repeating* it is
harmless with no idempotency key at all. Each step already answers a repeat with
what it produced the first time, so the composition does too — an agent that cannot
tell whether its submission landed simply submits again, and `created: false` says
which it was. A task already captured in a package carrying *different* work is a
conflict, not a repeat.

`feature status` reports every task of one plan revision with the package, release
unit, and release time attached to it. It reports each task's status verbatim rather
than a ready flag: only the caller knows what it is waiting for, and `blocked` and
`cancelled` have to be distinguishable from "not yet", which a boolean cannot do. A
blocked task also reports the status it was blocked out of, because blocked before a
push and blocked after one are different situations.

`plan seal` fixes a feature's scope by appending a sealed copy of its newest
revision. Sealing appends rather than edits: packages, units, and attempts all name a
plan revision, so a stored revision has to mean one thing forever. The response
carries the new revision number, and an agent must carry it forward — the revision it
drafted against is not the revision it works against. Sealing an already-sealed plan
is a conflict.

`task cancel` is explicit and durable: it accepts a required human explanation,
cancels a task that has not been published, blocks every dependent task, and
invalidates affected validation evidence without deleting its audit trail. It
never cancels published work or silently releases a dependent whose prerequisite
will not arrive.

`queue audit` reconciles crash evidence with durable state. A fully authenticated
package that was written before its database row is re-registered, and a complete
partial directory is promoted atomically. Incomplete, invalid, missing, or unsafe
entries are retained as recovery issues for inspection; the daemon never deletes
them during recovery. `queue export` writes a new portable directory containing a
consistent SQLite backup, only immutable packages that pass verification, and an
export manifest containing the database SHA-256. It excludes mutable workspaces, local API
tokens, and publication credentials. Import/restore into another installation is a
later workflow, so an export itself cannot enable a second publisher.

`release publish` runs one complete publication through the daemon-owned worker:
it fetches the current target, reconciles the captured package against it, reruns
the repository's trusted checks on the reconciled candidate, creates the candidate
commit with the supplied message and identity, and pushes it as an ordinary
fast-forward. Every stage is made durable before the operation it names, and the
candidate SHA and push intent are recorded before the remote is contacted, so a
crash mid-push is resolved against the remote rather than repeated as a second
commit. A competing push is reconciled onto, never forced over. Conflicts, changed
targets, failed checks, transport failures, and ambiguous remote states each stop
the attempt with a durable classification rather than retrying blindly.

`release attempt` shows one durable attempt, including its status, candidate SHA,
observed remote SHA, failure classification, and the reason it stopped.
`release attempts` lists recent attempts, optionally restricted to one package with
`--package-id`. Both read durable state, so an attempt remains inspectable long
after the process that created it exited.

`release create-unit` groups tasks that cannot independently leave the target
usable into one release unit; a task whose only remaining dependency is that its
prerequisite be *captured* is coupled into the same unit, while a task waiting for
a prerequisite to be *published* stands on its own. `release show-unit` inspects a
previously created unit.

`schedule set-policy` validates and activates one scheduling-policy revision for a
repository from the portable JSON contract documented in
[`SCHEDULE_POLICY_FORMAT.md`](SCHEDULE_POLICY_FORMAT.md); activating a later
revision changes future selection only. `schedule show-policy` inspects the active
policy. `schedule unit` selects a durable future release time for one captured
release unit against its repository's active policy, moving the unit's tasks from
`queued` to `scheduled`; calling it again for the same unit returns the existing
slot rather than drawing a new one, so a restart or a retry never redraws a
selection already made. `schedule show` inspects a previously selected slot without
drawing one.

`schedule withdraw` returns one unit's work to the queue so a fresh time can be
selected; `schedule recalculate` does the same for every live selection in a
repository, which is what a changed policy calls for. Neither deletes anything: a
withdrawn selection is retained with the reason it stopped being valid, so what
moved and why stays auditable. A unit whose attempt has already started is left
alone and reported as retained rather than disturbed — once an attempt owns the
work, its identity is not a schedule change's to discard. Cancelling or superseding
a task withdraws the affected units' selections automatically for the same reason,
and reports which units moved.

`schedule catch-up` applies the repository's missed-window behaviour to every
release time that has already passed — the case where the machine was asleep,
offline, or simply not running through a window. Under the default
`reschedule_forward`, nothing is released immediately and every overdue unit is
given a new future time, so a multi-day gap cannot turn into a burst. Under
`catch_up`, only as many units as `max_releases` allows are left due for immediate
release, oldest first, and the rest still move forward. A replacement time is
always drawn from the present moment onward, so an offline gap can never produce a
commit dated earlier than the moment it was actually made.

`schedule set-override` refines one repository's scheduling without changing the
policy it shares — any subset of the policy's fields, in the same JSON shape. The
override is stored unresolved and combined with whatever policy is active at the
moment a selection is made, and the combination is validated as a whole, so an
override cannot quietly produce an invalid effective policy.

`schedule due` lists units whose release time has arrived, taken fairly across
repositories rather than draining one at a time: each repository gets a turn before
any repository gets a second, so a repository with a deep queue, or one that keeps
failing and retrying, delays only itself. `--concurrency-limit` bounds how much work
is listed at once, which is what keeps a large backlog from becoming a burst of
simultaneous pushes.

`schedule preview` reports a repository's upcoming release times, and whether it is
paused, without changing anything.

`schedule pause` stops a repository from *starting* new releases and records why;
`schedule resume` lifts it. The distinction matters: pausing governs what begins,
never what is already running. An attempt that has transmitted a push has an
outcome on the remote that still has to be resolved, so pausing leaves it alone
rather than orphaning it — and a unit is only ever handed out as due while every
one of its tasks is still merely scheduled, so nothing an attempt already owns can
be claimed twice. The pause is durable, so a daemon restart does not quietly resume
publishing an operator deliberately stopped.

`schedule release-now` moves one unit's release time to the present. It overrides
the schedule, not the rules: the same eligibility checks apply, so a unit whose
prerequisites have not reached their required milestone is refused rather than
released early. To reschedule rather than release, use `schedule withdraw`
followed by `schedule unit`.

Enrollment refuses a second checkout of the same canonical remote that targets the
same branch. Two checkouts publishing to one ref are two writers racing for it, so
this is rejected at enrollment rather than surfacing later as a conflict. The live
release lease is keyed on the remote and target themselves, not on the repository
profile, for the same reason.

Once a repository has a schedule policy and a release unit has a selected time, the
installed service publishes it without any further command. The same maintenance
pass that notices a slot is due acts on it: the commit message is the plan's task
name, and the author is the identity the enrolled checkout already uses for its own
commits, so a scheduled commit is attributed exactly as a manual one would be. A
repository with no `user.name` and `user.email` configured is skipped rather than
attributed to an invented author.

`diagnose` answers whether one repository could publish right now, before a
release is attempted rather than after it has been blocked. It checks the remote
with a read-only `ls-remote`, which proves the stored credential is accepted
without creating a ref or a commit, and checks signing by confirming the
configured key is actually present. Background publication can never stop on a
prompt — terminal prompts are disabled, askpass points at a program that always
fails, and every invocation is bounded by a timeout — so a missing credential
fails in seconds. This command exists so the reason is legible while somebody is
there to read it.

An unreachable host is never reported as a rejected credential: sending someone to
re-authenticate because a server was down is a real cost, so only an actual refusal
is called one.

`integrations` lists external endpoints that are currently failing, how many
consecutive failures each has had, and when it may be retried. Health is tracked
per endpoint *and* per repository, so an unreachable Git remote delays only that
repository's Git work — a different repository keeps publishing, and notification
delivery is unaffected. Backoff doubles to a ceiling so a long outage costs a
bounded number of attempts rather than steady polling, and it is durable, so a
restart during an outage resumes the wait instead of retrying immediately.

An endpoint reported with no scheduled retry will not recover on its own. That is
reserved for faults waiting cannot fix — a rejected credential, a refused push —
because repeatedly presenting a rejected credential is how an account gets locked,
or how a credential prompt appears with nobody present to answer it.

`service install` registers the daemon as a macOS user-session agent so it keeps
running after the terminal is closed. It runs in the user's own login session
rather than as a system daemon on purpose: publishing uses the user's existing Git
credentials and SSH agent, which a root-owned daemon would either lose access to or
need a privileged copy of. The definition sets it to start at login and restart
after a crash, and deliberately sets no timer — the daemon reconciles durable
deadlines when it starts and when it wakes, rather than depending on a timer that
does not fire while the machine is asleep.

`service uninstall` stops the service and removes its definition but never deletes
the state directory. Removing the service is not the same decision as discarding
captured work that has not been published. `service status` reports whether it is
installed and loaded; `service show` prints the definition without installing
anything.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Command completed successfully |
| 1 | Unexpected internal or output failure |
| 2 | Invalid command-line usage |
| 10 | Actionable input, repository, policy, or diagnostic failure |
| 11 | Local service or dependency is temporarily unavailable |
| 12 | Requested state conflicts with an existing record |
| 13 | CLI/service protocol or authentication is incompatible |

Repository enrollment currently verifies the local path is a Git worktree, resolves
`origin` unless `--remote` is provided, validates target/mode combinations, and
rejects credential-bearing remote URLs. Full provider, signing, hook, LFS, submodule,
fork, and protected-branch capability checks are later implementation tasks.
