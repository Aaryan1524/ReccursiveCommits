# Agent handoff protocol

This is the contract between a terminal AI session and the service. It assumes
nothing about which agent is running: the agent is whatever process holds the
repository checkout and writes code, and every step below is an ordinary CLI
invocation with `--json`. There is no vendor SDK, no plugin, and no daemon-side
knowledge of any particular tool.

The supported Codex package is documented in [CODEX.md](CODEX.md). It is an
instruction layer over this contract; it does not add a separate transport,
daemon, or publishing path.

The protocol exists because a session that writes code and a service that
publishes it have to agree on three things they cannot otherwise see: which plan
the work belongs to, which base commit it was written against, and when a unit of
work is actually complete. Guessing any of them from file-watcher events is what
this replaces.

## Properties

**Versioned.** Every request carries `api_version`, and the daemon refuses a
version it does not implement rather than interpreting an unfamiliar payload. The
CLI surfaces that as exit code 13, which an agent should treat as "upgrade", not
"retry". The current version is 13.

**Idempotent.** Any step that changes state accepts `--idempotency-key`, which
makes a repeat of that step return the first result instead of acting again. See
[`CLI.md`](CLI.md#repeating-a-request-safely) for the exact semantics, including
what happens to a key when the first attempt fails.

**Vendor-neutral.** The transport is JSON over a Unix socket in the user's own
state directory, authenticated by a file-permission-protected token. Anything that
can run a subprocess can speak it.

**Reproducible.** Plans, packages and attempts are durable and content-addressed;
re-reading a step returns what it returned before, and the daemon owns the
workspace so two sessions cannot disagree about the base commit.

## The sequence

### 1. Plan registration

```sh
reccursive --json plan import feature-plan.json
```

The plan is the authority on what work exists and what depends on what; its format
is [`PLAN_FORMAT.md`](PLAN_FORMAT.md). Imports are append-only — revision 1, then
each next revision for the same feature and repository — so an agent that revises
its intent leaves the earlier intent inspectable rather than overwriting it. A
sealed plan is one the agent is declaring complete in scope.

A plan may be imported as a draft (`"sealed": false`) while the agent is still
deciding scope; nothing can be worked on until it is sealed, because an owned
workspace requires a sealed revision. Seal it with:

```sh
reccursive --json plan seal feature_<uuid>
```

This **appends** a sealed copy as revision N+1 rather than editing the draft, because
packages, units, and attempts all name a plan revision and a stored revision has to
mean one thing forever. The response carries the new number, and every later step
must use it.

Read it back with `plan show` (exact revision with `--revision`, newest by
default) or `plan history`.

### 2. Task start and base identification

```sh
reccursive --json workspace create feature_<uuid> --revision 1
```

This is the step that removes the guesswork. The daemon checks the plan's exact
target commit out into storage it owns and returns the path. The agent writes
there — not in the user's checkout, which is never switched, staged, or written
to. The returned workspace *is* the base identification: there is no separate
negotiation about which commit the work applies to, because the service chose it.

An uncommitted file from the user's checkout is carried in only when its
repository-relative path is named with `--include`. Everything unmentioned stays
where it is.

`workspace show feature_<uuid> --revision 1` re-reads the same answer.

### 3. Capture request

```sh
reccursive --idempotency-key "<agent>/capture/task_<uuid>" --json \
  package capture feature_<uuid> --revision 1 --task task_<uuid>
```

This is the agent declaring a task complete. Capture snapshots the whole owned
workspace for the named tasks into an immutable package: a self-contained Git
bundle plus an authenticated manifest of exact base and result trees. It is the
step most worth keying, because it is the one an agent is most likely to retry and
the one where retrying twice would otherwise produce two packages of the same work.

Capture is also where the service stops trusting the agent. Content policy runs on
the prospective tree, and daemon-owned trusted checks run inside the isolated
workspace; either can refuse the capture. A refusal names the rule and the
repository-relative path, never the matched content.

### 4. Validation status

```sh
reccursive --json feature status feature_<uuid>
reccursive --json package show package_<uuid>
reccursive --json release attempts --package-id package_<uuid> --limit 10
reccursive --json release attempt attempt_<uuid>
```

`feature status` is the one to poll: every task of a plan revision with its durable
status, and the package, unit, and release time attached to it. Statuses are reported
verbatim rather than as a ready flag, because only the caller knows what it is waiting
for — and `blocked` and `cancelled` have to be distinguishable from "not yet". A
blocked task also reports the status it was blocked *out of*.

`package show` recalculates the bundle and manifest hashes before answering, so a
corrupted package is an error rather than a stale success. `release attempt` and
`release attempts` read durable publication attempts: status, candidate SHA,
observed remote SHA, failure classification, and the reason it stopped. These
survive the process that created them, which is what lets an agent that was
restarted find out what happened without re-running anything.

### 5. Task completion and queueing

The whole submission is one command:

```sh
reccursive --json task submit feature_<uuid> --revision 2 --task task_<uuid>
```

It captures, groups, and schedules in one step, and returns all three results with
`created` saying whether this call produced them or found them already there.
**Repeating it is safe with no idempotency key**, which is the point: an agent that
cannot tell whether its submission landed submits again and gets the same package,
the same unit, and the same release time. Use a key on top of this only when you also
want a dropped-connection retry to replay the exact response.

From here the agent is finished. Nothing it does causes the publication: the daemon's
own maintenance pass acts on the schedule, including after the machine has been
asleep.

The three steps are also available individually, which is what `task submit` calls:

```sh
reccursive --idempotency-key "<agent>/unit/task_<uuid>" --json \
  release create-unit feature_<uuid> --revision 1 --task task_<uuid>
reccursive --json schedule unit unit_<uuid> --package-id package_<uuid> --revision 1
```

A release unit groups tasks that cannot independently leave the target usable. A
task whose only remaining dependency is that its prerequisite be *captured* joins
the same unit; a task waiting for a prerequisite to be *published* stands on its
own.

`schedule unit` selects the durable release time against the repository's active
policy. Calling it again for the same unit returns the existing slot rather than
drawing a new one. `release create-unit` behaves the same way: grouping the same work
again returns the group it is already in. A request that overlaps an existing unit
without matching it is a different grouping of grouped work, and is refused.

### 6. Reading the outcome

```sh
reccursive --json schedule show unit_<uuid>
reccursive --json schedule preview repo_<uuid>
reccursive --json release attempts --limit 10
reccursive --json logs --limit 50
```

## Exit codes

An agent should branch on these rather than on message text.

| Code | Meaning | What an agent should do |
| --- | --- | --- |
| 0 | Success | Continue |
| 2 | Usage error | Fix the invocation; do not retry as-is |
| 10 | Action required | A person or a plan change is needed |
| 11 | Service unavailable, or a keyed request still running | Retry, with backoff |
| 12 | Conflict | Re-read state before acting; do not repeat blindly |
| 13 | Incompatible service version | Upgrade; retrying will not help |
| 1 | Internal error | Report it; retrying is unlikely to help |
