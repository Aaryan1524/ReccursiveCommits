<div align="center">
  <h1>ReccursiveCommits</h1>
</div>

<div align="center">
  <h3>Finish the work today. Let the commit be made on the schedule you chose — without you at the keyboard.</h3>
</div>

<div align="center">
  <img src="https://img.shields.io/badge/platform-macOS-black" alt="Platform: macOS">
  <img src="https://img.shields.io/badge/network%20stack-none-brightgreen" alt="No HTTP or TLS dependency">
  <img src="https://img.shields.io/badge/rust-stable-B7410E" alt="Rust, stable toolchain">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT licensed">
</div>

<br>

You capture a change once, verified, and hand it to a local queue. A background
service picks a release time inside the window you configured, reconciles
against whatever the remote has moved to since, creates the commit at that
moment, and pushes it. Nothing is backdated and no empty commit is ever made —
the commit is created when it is published, because a real change published on
a schedule is the only thing here worth having.

```bash
cargo build --workspace --release
./target/release/reccursive setup ~/code/your-project
./target/release/reccursive service install
```

> [!NOTE]
> This runs entirely on your machine, as you. There is no account, nothing
> listens on a network port, and Git credentials are never stored — it uses the
> credential helper or SSH agent you already have.

## Why use it

- **It publishes without you** — the service is a launchd agent, so closing the
  terminal does not stop it and a crash restarts it. It runs in your login
  session, which is what lets it publish with the Git credentials you already
  have, and which also means logging out or shutting down stops it until you log
  in again. Sleep the machine through a release time and the work is published
  shortly after it wakes — late, but never silently skipped.

- **The commit is created at release time** — not written now and held back, and
  never backdated. What lands is the change you captured, applied to whatever
  the target has become in the meantime.

- **A conflict blocks, it does not guess** — the candidate is rebuilt against
  the current target and verified before any push. If it cannot apply cleanly,
  the unit stops and tells you which paths, rather than publishing something
  nobody wrote.

- **A protected branch is respected** — a refused push is never retried into
  submission and never reported as done. You are pointed at a branch the rules
  actually permit, or at the pull-request strategy.

- **Nothing merges itself** — the pull-request strategy opens the request on
  your schedule. Merging is authority over what lands on your main branch, and
  this product does not take it.

- **No network stack of its own** — `git` and `curl` are driven as processes
  with explicit timeouts and no shell, so TLS, proxies and your system trust
  store stay where they already work.

```
capture        →  queue         →  schedule      →  publish
(verified,        (durable,        (your window,    (commit created
 immutable)        local SQLite)    a real time)     at that moment)
```

---

## Documentation

- **[docs/CLI.md](docs/CLI.md)** — every command, what it refuses and why.
- **[docs/PUBLICATION.md](docs/PUBLICATION.md)** — direct push versus pull
  request, what the GitHub token can and cannot do, and which suits you.
- **[docs/SECURITY.md](docs/SECURITY.md)** — where secrets live, what never
  leaves the machine, and the findings from the pre-distribution review,
  including the ones judged not to be problems.
- **[docs/PLAN_FORMAT.md](docs/PLAN_FORMAT.md)** — the feature plan schema:
  phases, tasks, dependency milestones, acceptance checks.
- **[docs/SCHEDULE_POLICY_FORMAT.md](docs/SCHEDULE_POLICY_FORMAT.md)** — release
  windows, daily limits, spacing, and what happens to a missed window.
- **[docs/AGENT_HANDOFF.md](docs/AGENT_HANDOFF.md)** — the six-step protocol an
  AI agent follows to hand finished work to the queue idempotently.
- **[docs/CODEX.md](docs/CODEX.md)** — using it from Codex.

## Layout

```
ReccursiveCommits/
├── crates/
│   ├── core/          domain model and state rules (no I/O)
│   ├── store/         durable local persistence (SQLite, WAL, owner-only)
│   ├── protocol/      versioned local API contract shared by CLI and daemon
│   ├── git/           safe noninteractive Git process adapter
│   ├── github/        optional pull-request adapter (nothing else depends on it)
│   ├── capture/       owned workspaces and immutable snapshot packages
│   ├── daemon/        queue owner, scheduler, release worker, local service
│   └── cli/           the `reccursive` command
├── tests/
│   ├── scenarios/     22 end-to-end scripts, run in CI on macOS and Linux
│   └── manual/        the sleep test, which needs a real sleeping machine
└── integrations/
    └── codex/         agent skill definition
```

### Why the Git adapter is its own crate

Every Git call goes through it as an argument list — never a shell string —
with a timeout, terminal prompts disabled, and `askpass` pointed at a program
that always fails. A background service must never stop on a passphrase prompt
with nobody there to answer it, so a missing credential fails in seconds instead
of hanging forever. Keeping that in one crate is what makes the rule checkable.

### Why the GitHub adapter is optional and separate

The pull-request strategy is the only part that talks to anything but Git. It
lives in its own crate that nothing else depends on, so the pure-Git path stays
whole for GitLab, Bitbucket, Gitea, and a bare repository on a server you own.
It speaks HTTP by driving `curl`, which is why there is no HTTP or TLS crate in
the dependency tree.

### Why scenarios exist alongside unit tests

This codebase's recurring defect is tested library code that nothing calls — a
unit test passing while the path a user reaches is broken or absent. It has been
found ten times. Each scenario script drives the real CLI against a real daemon
over a real socket and asserts what is *durable* afterwards, which is the only
way that class of bug shows up. See
[tests/scenarios/README.md](tests/scenarios/README.md) for the conventions,
including the SIGPIPE trap that bit the suite twice.

## Setup

### 0. Prerequisites

Git, and a Rust stable toolchain. Everything else is vendored — SQLite is
compiled in, and there is nothing to `pip install` or `npm install`, ever.

```bash
cargo build --workspace --release
./target/release/reccursive doctor    # git, state directory, service connectivity
```

### 1. Enrol a repository

Start with a throwaway repository, not one that matters to you.

`setup` does enrollment, a schedule policy, and the service in one pass:

```bash
./target/release/reccursive setup ~/code/your-project
```

It refuses rather than guessing when it is not attached to a terminal, and
**your working tree is never touched** — uncommitted work, untracked files and
staged changes are all left exactly as they are, at every stage.

To choose where and how work is published instead of taking the defaults, see
[docs/PUBLICATION.md](docs/PUBLICATION.md). The short version: direct push needs
nothing configured; the pull-request strategy needs a GitHub token you create
and can revoke.

### 2. Install the background service

```bash
./target/release/reccursive service install
./target/release/reccursive service status   # should report installed and loaded
```

This writes a launchd agent labelled `com.reccursive.reccursive-daemon` that
runs as you, keeps its state in
`~/Library/Application Support/ReccursiveCommits`, and starts again at login.
Run `service show` first if you want to read the definition before installing
it.

### 3. Give it work

A plan describes what you are delivering: phases, tasks, what each task depends
on, and how you will know it is done. `plan template` writes one already wired
to your repository, with real identifiers in it.

```bash
reccursive plan template <repository-id> -o plan.json
# edit plan.json: the goal and the task names
reccursive plan import plan.json
reccursive plan seal <feature-id>        # fixes the scope; appends the next revision
```

**Sealing bumps the revision.** `import` stores revision 1, `seal` appends
revision 2, and everything after this uses the sealed number. A workspace can
only be built against a sealed revision, because the workspace and every package
captured in it belong to a scope that must not change underneath them.

Then do the work — **in the workspace it prints, not in your checkout**:

```bash
reccursive workspace create <feature-id> --revision 2
#   → prints a path under the state directory. Edit files there.

reccursive package capture <feature-id> --revision 2 --task <task-id>
reccursive release create-unit <feature-id> --revision 2 --task <task-id>
reccursive schedule unit <unit-id> --package-id <package-id> --revision 1
```

An agent collapses those last three into `reccursive task submit <feature-id>
--task <task-id>`; see [docs/AGENT_HANDOFF.md](docs/AGENT_HANDOFF.md).

```bash
reccursive queue status
reccursive schedule preview <repository-id>
```

> [!TIP]
> To see a publish immediately instead of waiting for your window, activate a
> policy whose window is all day, then `reccursive schedule release-now <unit>`.
> The default policy is weekday working hours, so outside those a scheduled unit
> is waiting rather than stuck.

### 4. Prove it publishes while the machine sleeps

This is the claim worth testing, and it cannot be automated — it needs a real
sleeping Mac. The script builds a throwaway repository and bare remote, enrols
it, captures a unit, and schedules it far enough out that you can shut the lid.
**It touches no repository of yours.**

```bash
./tests/manual/phase5_sleep_test.sh          # sets everything up, then stops
# close the terminal, sleep the machine past the printed time, wake it
./tests/manual/phase5_sleep_test.sh check
```

## Usage

| Command | What it answers |
| --- | --- |
| `status` | Is the service up, and what is queued |
| `queue status` | Every unit, where it stands, and what needs a person |
| `queue watch` | The same, redrawn until you stop it |
| `schedule preview <repo>` | When the next release is expected |
| `diagnose <repo>` | Can this repository publish right now, and if not, which part |
| `doctor` | Are the local prerequisites and the service healthy |
| `release attempt <id>` | Why a publication stopped, and the command that addresses it |
| `contributions <repo>` | What decides whether published work counts, and what does not |
| `queue audit` | Package storage integrity and unresolved crash-recovery evidence |
| `queue export <dir>` | A portable backup, leaving the live queue untouched |
| `logs --limit 100` | Recent sanitized daemon events |

Every blocked publication ends with a **Next:** block naming the command that
addresses it. That mapping is exhaustive over an enum, so a failure with no
guidance does not compile.

## Known gaps

- **macOS only.** The service is a launchd agent. Linux and Windows are not
  supported yet and the CLI is not tested on them; the scenario suite runs on
  Linux in CI, but service installation does not.

- **No signed binaries yet.** You build from source. There is no Homebrew
  formula and no notarized archive, so there is nothing to install on a machine
  without a Rust toolchain.

- **The launchd install path is not covered by CI.** Scenarios run against
  fixture state directories and deliberately never touch
  `~/Library/LaunchAgents`, so `service install` is covered by unit tests and
  the manual sleep test rather than end to end. It is the most likely place to
  hit friction first.

- **No notifications.** Nothing tells you a release happened except the queue
  and the log. `queue watch` is the closest thing.

- **One machine.** Restoring a queue backup onto a second machine and running
  both would publish the same work twice; nothing currently prevents that.

- **Contribution graphs are GitHub's, not ours.** A release time is when this
  service publishes. It is not a promise about a green square, and
  `contributions` will not pretend otherwise.

- **No screenshots in this README.** A terminal capture of `queue status` with
  a mixed queue, and one of the `Next:` block on a blocked attempt, would show
  the product better than any paragraph here does.

## Troubleshooting

```bash
reccursive doctor                    # prerequisites and service connectivity
reccursive diagnose <repository-id>  # can this repository publish right now
reccursive queue status --needs-attention
reccursive logs --limit 100
launchctl list | grep reccursive     # should show the agent loaded
```

**`state_directory  action_required  ... is missing or not owner-only`** — the
state directory does not exist yet, or its permissions were widened. Installing
the service creates it `0700`. Everything inside it — the socket, the database,
the local API token, any GitHub token — is owner-only by design.

**`Can publish now: no` with `Checkout: missing`** — the enrolled working copy
has been moved or deleted. Credentials and signing are probed against the
managed mirror, which outlives your checkout, so this is the line that tells you
the truth. Nothing publishes from that repository until it is back, and the
daemon records `checkout_unattributable` in `logs` rather than looking idle.

**A unit sits at `blocked` with `branch_rule`** — the remote answered and
refused the push: a protected branch, a required review, or a pre-receive hook.
Retrying cannot change that answer, so nothing retries it. `release attempt <id>`
names a branch the rules do permit.

**A unit sits at `awaiting your merge`** — a pull request is open and waiting
for you. That is the strategy working, not a fault. The queue prints its number
and URL.

**Nothing published while the machine was asleep** — the maintenance pass timer
is suspended during sleep, so the work is caught shortly after waking rather
than at the scheduled moment. Wait out a pass or two before concluding anything
is wrong; the sleep test does this for you.

## Uninstall

```bash
reccursive service uninstall              # stops and unloads the agent, keeps queued work
rm -rf ~/Library/"Application Support"/ReccursiveCommits   # removes the queue and all credentials
```

If you used the pull-request strategy, **revoke the GitHub token on GitHub as
well** — removing the local copy does not revoke it.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The one thing worth knowing up front:
a change that adds library code should add the scenario that reaches it, because
code nothing calls is the defect this project keeps finding in itself.

## License

MIT. See [LICENSE](LICENSE).
