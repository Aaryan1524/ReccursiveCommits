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
Human-readable list commands use headed tables. Cells are kept to a bounded
single line so one unusually long path or diagnostic does not hide neighboring
rows; use `--json` when a caller needs complete, untruncated values.

## Setting up a repository

`setup` is the guided first run. It checks what has to be true, asks what it
cannot infer, and then enrolls the repository, activates a schedule policy, and
optionally installs the background service:

```sh
reccursive setup /path/to/repository
```

Everything it does is a command you could issue yourself; what it adds is order,
defaults, and a preflight. The most valuable check is the one for a committer
identity: a checkout without `user.name` and `user.email` enrolls perfectly well
and then never publishes, because the release pass skips a unit it cannot
attribute and does so without an error anyone sees. Setup refuses instead, while
there is someone to tell, and prints the two `git config` commands that fix it.

For scripts, pass `--non-interactive` and supply what cannot be inferred:

```sh
reccursive setup /path/to/repository --non-interactive --timezone America/New_York
```

Without a terminal and without `--non-interactive`, setup **refuses** rather than
falling back to defaults. Falling back would mean silently accepting a target
branch, a schedule, and a publication mode nobody chose. `--mode immediate`
requires `--development-target`; a repository whose `origin` has no readable URL
requires `--remote`.

The schedule is the built-in weekday policy — weekday afternoons, up to three
releases a day, 45 minutes apart — in this machine's time zone unless
`--timezone` says otherwise. Use `--schedule FILE` to activate your own document
in the format described in
[`SCHEDULE_POLICY_FORMAT.md`](SCHEDULE_POLICY_FORMAT.md), or `--no-schedule` to
enroll without one.

Setup never publishes anything. If a later step fails after an earlier one
succeeded, nothing is rolled back: enrollment is atomic on its own, and quietly
reversing a repository you may already be using would be worse than reporting
what stands and naming the commands that finish the job.

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

## Preparing a plan

`plan template <repository>` writes a starter document that is already valid —
a working two-task plan with a real dependency between them, not a skeleton of
placeholders, because the fastest way to learn the format is to change something
that already imports.

`plan check FILE` validates a document locally, using the same
`FeaturePlan::validate` the daemon runs at import. Nothing is sent anywhere, so
an author can iterate without touching the service, and a document this accepts
is a document import accepts.

`plan show` is a review rather than a summary. Approving a plan means agreeing
to what it will publish, so it prints every phase and task, what each task waits
for and at which milestone, and what it claims will prove the task is done. A
count of phases is not something anyone can approve.

`plan export` writes a stored revision back out as a document — for editing
elsewhere, for review, or for keeping alongside the code.

`plan edit` opens the newest revision in `$VISUAL` or `$EDITOR` and appends the
result as the next revision. It **never writes back to what is stored**: a
stored revision is immutable, because packages, units, and publication attempts
all name the revision they were built from. An edit reads revision N and imports
what comes back as N+1, so already-published work is untouched by construction
rather than by care. The appended revision is a draft even when the one it was
derived from was sealed — an edit is a change of scope, and inheriting the seal
would publish that change without anyone agreeing to it. An edit that changes
nothing appends nothing, and an edit that produces an invalid plan imports
nothing.

Sealing is the approval step. `plan seal` fixes scope, and an owned workspace
requires a sealed revision, so nothing can be built against a plan nobody has
committed to. Review with `plan show`, then seal.

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

`package changes` shows what a captured package changes relative to the base it
was captured against: which files, and with `--patch`, the patch itself. The
patch is absent by default because a release unit's diff can be large and the
usual question is answered by the file list; when it is included and exceeds the
size limit it is cut short and *says so*, rather than presenting a partial diff
as a whole one. The package is an immutable bundle, so this reconstructs it into
throwaway storage and asks Git — nothing is written where the package lives, and
the reconstruction is verified against the manifest before it is trusted.

`package checks` shows the checks that actually ran: the command, whether it
passed, failed, or never returned, and its scrubbed output. Checks appear in two
groups — at capture, in the package's own workspace, and at release, against
each reconciled candidate, since reconciliation changes the parent commit and so
the checks run again. A candidate result that no longer stands is kept with the
reason it was invalidated rather than overwritten, so the history of what was
verified against what stays answerable.

`task cancel` is explicit and durable: it accepts a required human explanation,
cancels a task that has not been published, blocks every dependent task, and
invalidates affected validation evidence without deleting its audit trail. It
never cancels published work or silently releases a dependent whose prerequisite
will not arrive.

`queue status` shows queued release work across every enrolled repository, or
one with `--repository-id`: each release unit, the plan task names it carries,
where it stands, and when it is due. States are `ready` (captured and grouped,
no time selected), `scheduled`, `due now`, `publishing`, `blocked`, `cancelled`,
and `published`. There is deliberately no *awaiting-merge* state: that belongs to
pull-request lifecycles, which arrive in Phase 10, and a value nothing can
produce would be a lie in the interface. Anything blocked, and any release time
that was withdrawn, is called out beneath the table with its reason, because
those are the only lines that ask for an action.

When standard output is a real terminal, each state in that table is prefixed
with a small icon and colored — green for `published`, yellow for anything
about to happen or waiting on you, red for `blocked`, dim for `ready` and
`cancelled`. This is presentation only: it disappears the moment output is
piped, redirected, or `NO_COLOR` is set (<https://no-color.org>), and it never
changes the underlying label a script or `--json` caller sees.

`queue watch` redraws that view on an interval. It is a *display*: it holds no
lease, claims no work, and tells the daemon nothing — it asks the same question
`queue status` asks, repeatedly. Stopping it therefore cannot affect what is
queued or whether the service publishes, which is structural rather than
careful, since the CLI is a protocol client with no authority to stop anything.
`--for SECONDS` bounds a watch so a script can use one; a closed pipe ends it
without an error. Forecasts of upcoming release times remain `schedule preview`.

`schedule history <unit>` lists every release time ever selected for a unit and
why each stopped being valid. Withdrawn selections are retained rather than
deleted, so what moved and why stays answerable long after the fact.

`queue status` accepts `--state` to show one kind of unit, and
`--needs-attention` for the two that ask something of a person: blocked work,
and work whose release time was withdrawn. Filtering happens in the CLI, because
it is a question about presentation rather than about state — the same summary
answers every filter.

`release attempt` ends with a **Next:** block naming the commands that address
whatever blocked it: `package checks` for a failed check, `package changes
--patch` for a conflict, `diagnose` for a credential, `integrations` for a
remote that is merely unreachable. Where a failure genuinely needs a person to
look at the remote — an ambiguous push — it says that instead of suggesting a
command. A classification with no known action prints nothing rather than
something plausible.

A target branch that refuses direct pushes is the one worth calling out.
`branch_rule` means the remote answered and said no: a protected branch, a
required review, or a pre-receive hook. Retrying an identical push cannot change
that answer, so nothing retries it and nothing is reported as published — the
unit stays blocked and the target is untouched. The **Next:** block names a
branch the rules do permit, by enrolling the repository in immediate mode with a
development branch, rather than asking anyone to weaken a protection. See
[PUBLICATION.md](PUBLICATION.md) for which strategy suits a given repository.

Every classification the release worker can record has an action, and that is
enforced rather than maintained: `ReleaseFailure` is an enum and the mapping
matches on it exhaustively, so a new failure without guidance does not compile.

`schedule` with no subcommand works from whichever source has work. If your own
checkout has uncommitted changes and nothing has been prepared through the plan
flow, it shows those changes, asks what to call them, and schedules them — no
plan, revision, package or identifier involved. It reads your checkout and never
writes to it: the snapshot is taken into the daemon's own storage, so editing
afterwards cannot change what was scheduled, and nothing is ever reset, stashed,
staged or committed on your behalf. Ignored files are excluded, because git
excludes them. A file you deleted is captured as a deletion and published as one,
rather than being resurrected from the last commit. Renames are refused for now
rather than captured wrongly.

If prepared work exists as well, it asks which you mean. The two are never
combined into one release: a release unit's tasks must match a captured
package's exactly.

`schedule` with no subcommand is the interactive scheduler: it shows work that is
ready, lets you pick what ships together, asks for a date and a time in the
repository's own zone, shows you the batch, and schedules it. It is for a person
at a terminal — `--json` refuses it outright, and so does a pipe, because a
prompt nobody can answer is worse than an error. Everything it does is the same
`release create-unit` and `schedule unit` an automated caller would send.

The wizard checks a chosen time **before** showing you the batch to confirm, so
a refusal arrives at the prompt rather than after you have reviewed and said
yes. A rejected weekday asks for the date again; a rejected hour or a spacing
conflict asks only for the time, keeping the date you already gave.

That check is advisory — it reserves nothing. Scheduling validates again when
the slot is persisted, so a release someone else schedules in between is still
refused, and the wizard says what changed and asks again.

`schedule check REPOSITORY --at "2026-09-18 10:30"` runs the same check from the
command line, returning a structured verdict.

`ready` prints the same list non-interactively: work that has been captured, has
passed its checks, and is not yet grouped into a release unit.

`schedule unit --at "2026-09-18 10:30"` names an exact release time instead of
letting the policy choose one. `--zone` says which zone that clock time is
written in, defaulting to this machine's. **A requested time does not bypass the
schedule policy.** It is checked against the same rules the scheduler draws
within — allowed day, publishing window, minimum spacing, daily maximum — and
refused, with the hours that *are* open, when it does not fit. It is never
silently moved: asking for 10:30 and being given 14:05 would tell you a release
is scheduled without telling you when.

Times are shown the way you read a clock — `Tue 15 Sep 2026, 16:48 EDT`, in this
machine's zone. `--json` keeps the raw millisecond value, because that is the
stable contract other programs parse and a localised string would be neither.

`schedule release-now UNIT` moves a unit's release time to now. It is subject to
the same **dependency** rules as an ordinary selection — a prerequisite that has
not reached its milestone still holds it back — but not to the release window:
asking for something now is a decision a person is making now. A time asked for
this way is recorded as such, so missed-window reconciliation leaves it alone
rather than treating it as a window that was slept through.

A workspace can only be built against a **sealed** plan revision, because
sealing fixes the scope that the workspace and every package captured in it
belong to. `plan seal FEATURE` appends a sealed copy as the next revision, so
the revision number goes up — use the new one from then on.

`diagnose REPOSITORY` answers one question — can this repository publish right
now — and names whichever part cannot. **Checkout** is reported first and
deliberately: credentials and signing are probed against the managed mirror,
which outlives your working copy, so a checkout that has been moved, deleted, or
left without `user.name` and `user.email` would otherwise look perfectly healthy
while nothing could ever publish from it. A commit needs an author, and this
product will not invent one.

When the daemon skips a repository for that reason it records
`checkout_unattributable` in `logs`, naming the path and what to run. A queue
that is stuck should never merely look idle.

`repository set-policy REPOSITORY` changes where or how an enrolled repository
publishes. Only what you name changes; omitting a flag leaves that setting alone,
and removing the development branch has to be said out loud with
`--no-development-target` rather than implied.

It refuses, with the reason, when something is mid-flight under the old policy: a
publication already owns a unit, a pull request is open against the branch the
old policy named, or work sits on a development branch and has not reached the
target yet, where removing that branch would strand it. A refusal changes
nothing at all — the policy revision does not move.

What it does move is release times. A selected time was drawn under the old
policy and may now name a different branch, so live selections are withdrawn and
chosen again from the new policy. Each unit keeps its identity, so nothing is
duplicated, and nothing publishes in between. The command prints which units were
moved.

`contributions REPOSITORY` explains what this machine decides about a published
commit — the identity it will carry, the branch it lands on, and that its author
date is its release time, never backdated — and names what only GitHub can
decide, rather than guessing. It deliberately does not tell you that scheduling a
push for a date produces a contribution on that date: those are different things
decided by different systems.

`github set-token REPOSITORY` stores the GitHub token the pull-request strategy
uses, read from **standard input** — an argument would be visible in the process
list and saved in your shell history. It is written owner-only beside the
service's own credentials, and it never crosses the local API in either
direction: `github status` says whether one is stored, never what it is, and
`github forget-token` removes it. Revoke it on GitHub as well.

Only repositories enrolled with `--integration pull-request` use a token at all.
Direct push, which is the default, needs nothing configured. See
[PUBLICATION.md](PUBLICATION.md) for the difference.

`diagnostics export DIRECTORY` writes a shareable report: service status,
repositories, the queue, integration health, and recent events. Everything in it
is read back through the ordinary local API, which is what makes it safe to
share — events and check output are scrubbed for credential-shaped text when
they are stored, and enrollment already refuses a remote with embedded
credentials. The local API token, the queue database, and captured package
contents are never included, and the manifest says so. It is a copy: describing
the queue never consumes it. Review the files before sending them anywhere.

`schedule pause` takes a repository, or `--all` to stop every enrolled one.
Naming neither is a usage error rather than a guess. Pausing all of them is
several calls, since pausing is per repository and there is no durable
"everything is paused" state; if one refuses, the ones already paused stay
paused and are listed, because silently resuming them would undo what an
operator asked for.

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
