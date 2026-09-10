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

`queue audit` reconciles crash evidence with durable state. A fully authenticated
package that was written before its database row is re-registered, and a complete
partial directory is promoted atomically. Incomplete, invalid, missing, or unsafe
entries are retained as recovery issues for inspection; the daemon never deletes
them during recovery. `queue export` writes a new portable directory containing a
consistent SQLite backup, only immutable packages that pass verification, and an
export manifest containing the database SHA-256. It excludes mutable workspaces, local API
tokens, and publication credentials. Import/restore into another installation is a
later workflow, so an export itself cannot enable a second publisher.

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
