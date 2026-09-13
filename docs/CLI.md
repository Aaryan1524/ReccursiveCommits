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
cargo run -p reccursive-cli -- --state-dir /path/to/state plan show feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state plan history feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace create feature_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace create feature_<uuid> --include src/config.rs
cargo run -p reccursive-cli -- --state-dir /path/to/state workspace show feature_<uuid> --revision 1
cargo run -p reccursive-cli -- --state-dir /path/to/state package capture feature_<uuid> --revision 1 --task task_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state package show package_<uuid>
cargo run -p reccursive-cli -- --state-dir /path/to/state task cancel feature_<uuid> --revision 1 --task task_<uuid> --message "replaced by a newer approach"
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
cargo run -p reccursive-cli -- --state-dir /path/to/state queue audit
cargo run -p reccursive-cli -- --state-dir /path/to/state queue export /absolute/path/to/queue-backup
```

Pass `--json` for a stable JSON envelope with no prompts or spinners. The
`RECCURSIVE_STATE_DIR` environment variable can replace `--state-dir`.

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
