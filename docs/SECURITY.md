# Security boundaries

This describes what the service can reach, what can reach it, and where secrets
live. It is written for someone deciding whether to run this on a machine that
matters to them.

Findings from the pre-distribution review are at the bottom, including the ones
that were fixed and the ones judged not to be problems.

## What it runs as

One process, as you, on your machine. There is no server, no account, and
nothing listens on a network port. It talks to Git over the network the way your
own `git` does, using the same credentials, and — only if you choose the
pull-request strategy — to `api.github.com` with a token you created.

## The local API

The daemon listens on a Unix domain socket inside its state directory. Two
things guard it:

- **File permissions.** The state directory is `0700` and the socket is `0600`,
  so no other user on the machine can connect at all.
- **A token.** Every request carries a token generated at first start and stored
  `0600` beside the socket. Comparison is constant-time.

The second is defence in depth rather than the primary control: anyone who can
open the socket is already running as you. It exists so that a state directory
copied, restored, or placed somewhere more permissive does not become an open
door.

## Where secrets live

| Secret | Location | Mode |
| --- | --- | --- |
| Local API token | `<state>/auth.token` | `0600` |
| GitHub token (opt-in) | `<state>/credentials/github-<id>.token` | `0600` |
| Queue database | `<state>/state.sqlite` | `0600` |

Git credentials are **not** stored here at all. The service uses your existing
credential helper or SSH agent, so there is nothing of yours for it to lose.

Neither token crosses the local API. The GitHub token is written and read by the
CLI directly, and `github status` reports only whether one exists. The GitHub
adapter passes it to `curl` on standard input, never as an argument, so it does
not appear in the process list or your shell history.

## What never leaves the machine

- The queue database.
- Either token.
- Captured package contents, except as the commit you scheduled.

`diagnostics export` is built entirely from ordinary read-only API responses, so
it cannot contain something the API does not expose. Events and check output are
scrubbed for credential-shaped text — URL credentials, `Bearer` values, and
known token prefixes — when they are *stored*, not when they are displayed, so a
secret that reached an event was masked before it was written down.

## Running commands

The service runs two external programs: `git`, and `curl` when you have chosen
the pull-request strategy. Both are invoked as argument lists, never through a
shell, with an explicit timeout, terminal prompts disabled, and `askpass`
pointed at a program that always fails — so a missing credential fails in
seconds instead of waiting forever for an answer nobody is there to give.

Acceptance checks run as commands, and **they cannot be defined over the local
API**. One check is registered at enrollment (`git diff --check`) and there is
no request that adds another. A malicious plan therefore cannot cause a command
to run.

## Files and paths

Workspaces are created under the state directory, and every ancestor is checked
for symbolic links before anything is written. Capture goes through Git, which
records a symbolic link as a link rather than following it, so a link pointing
at something outside the repository is published as a link and not as the
contents of whatever it pointed to.

## Review findings

### Fixed

- **The queue database was world-readable (`0644`).** SQLite created it with the
  process umask, and the only thing protecting it was the `0700` directory above
  it. It and its write-ahead sidecars are now `0600`. Verified by a test that
  performs a write first, because the sidecars do not exist until one happens.
- **A GitHub token identifier was not validated.** `github set-token` accepted
  any string and interpolated it into a filename. Traversal happened to be
  blocked by the `github-` prefix — accident, not design — and the error was
  unreadable. The identifier is now a typed repository id at the CLI boundary
  and validated again in the storage layer.
- **A crafted remote URL could redirect API requests.** An owner of `..` would
  produce `/repos/../x/pulls`, which normalises to a different endpoint. Owner
  and repository names are now validated before they reach a URL.
- **Token comparison returned early.** Now constant-time.

### Considered and not changed

- **`AuthToken` is stored in plaintext.** Encrypting it would need a key, which
  would live beside it. File permissions are the real control.
- **The local API has no per-command authorization.** There is one user and one
  trust level by design; a second would be a permission model with nobody to
  administer it.
- **`{base_commit}` is substituted into a check command.** The value is a commit
  id read from the database, not from a request, and commands are argument lists
  rather than shell strings.

## Reporting something

Open an issue describing the class of problem. Please do not include a working
exploit, a token, or the contents of a real queue database.
