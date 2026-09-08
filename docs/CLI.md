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
```

Pass `--json` for a stable JSON envelope with no prompts or spinners. The
`RECCURSIVE_STATE_DIR` environment variable can replace `--state-dir`.

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
