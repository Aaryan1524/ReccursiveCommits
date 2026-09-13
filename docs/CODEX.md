# Codex integration

Reccursive ships a project-local Codex skill at
[`integrations/codex/reccursive`](../integrations/codex/reccursive). It teaches
Codex to use the public Reccursive CLI after a person has set up the repository
and sealed a feature plan; it does not give Codex a separate queue, permission
to publish directly, or permission to alter the user's checkout.

## Install for one project

From a clone of this repository, copy the skill into the project where Codex
will build planned work:

```sh
mkdir -p /path/to/your-project/.codex/skills
cp -R integrations/codex/reccursive /path/to/your-project/.codex/skills/reccursive
```

Restart the Codex session after installing so it can discover the skill.

## What the skill does

1. Creates or re-reads the daemon-owned workspace for the exact sealed plan
   revision.
2. Has Codex work only in that returned workspace.
3. Submits each completed, named plan task through `reccursive --json task
   submit`.
4. Reads `feature status` for the durable outcome instead of inferring it from
   file changes, commit history, or a file watcher.

The daemon—not Codex—runs trusted checks, selects the release slot, reconciles
remote changes, and publishes. A submission is repeat-safe, but a blocked or
conflicting response requires the person responsible for the plan or repository
to resolve it. See [the agent handoff contract](AGENT_HANDOFF.md) for the
versioned CLI protocol and exit-code behavior.
