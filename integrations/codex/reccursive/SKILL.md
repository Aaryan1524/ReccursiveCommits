---
name: reccursive
description: Submit completed work from a sealed Reccursive feature plan through the local Reccursive CLI. Use when Codex is asked to build a planned task and hand it to Reccursive for verified, scheduled publication; do not use to author plans or work outside a registered Reccursive workspace.
---

# Reccursive task handoff

Use this skill only after a person has enrolled the repository, configured its
release policy, and imported a sealed feature plan. The plan defines the allowed
scope; do not invent tasks, targets, release checks, or schedule permissions.

## Start in the owned workspace

Create the workspace for the exact feature revision and use the returned `path`
as the directory for all task work:

```sh
reccursive --json workspace create feature_<uuid> --revision <n>
```

Do not edit, stage, switch, commit, or push the user's original checkout. Do
not write outside the returned workspace. Re-read an existing workspace with
`workspace show` rather than creating another one.

## Finish one planned task

Implement and validate the task in the owned workspace. When it is complete,
hand it off with the exact plan task ID:

```sh
reccursive --json task submit feature_<uuid> --revision <n> --task task_<uuid>
```

`task submit` captures the immutable result, groups coupled work, and selects a
durable release time. It is safe to repeat without an idempotency key when the
same task set is submitted again. It does not publish immediately; the local
daemon publishes only when the selected release time is eligible.

After every submission, inspect the durable state rather than guessing from
files or Git history:

```sh
reccursive --json feature status feature_<uuid> --revision <n>
```

## Stop conditions

- Exit code `0`: report the returned package, release unit, and scheduled time.
- Exit code `11`: the local service is unavailable; retry with backoff after
  `reccursive --json doctor` reports it ready.
- Exit code `10` or `12`: stop changing work, inspect `feature status` and
  `reccursive --json logs --limit 50`, then ask the user to resolve the named
  plan, policy, or repository issue.
- Exit code `13`: stop and have the user restart or upgrade the local service.
- Any other nonzero exit: preserve the workspace and report the structured
  error; do not retry blindly.

For the complete wire and lifecycle contract, read
[`docs/AGENT_HANDOFF.md`](../../../docs/AGENT_HANDOFF.md).
