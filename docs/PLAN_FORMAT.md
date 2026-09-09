# Feature plan format

Feature plans are portable JSON documents produced by an agent or another client
and validated again by the daemon before storage. Version 1 requires a repository,
target branch, ordered phases, stable task IDs, dependency milestones, and at least
one observable acceptance check for every task.

```json
{
  "schema_version": 1,
  "feature_id": "feature_00000000-0000-4000-8000-000000000001",
  "revision": 1,
  "repository_id": "repo_00000000-0000-4000-8000-000000000002",
  "goal": "Ship repository health reporting",
  "target": "refs/heads/main",
  "sealed": true,
  "phases": [
    {
      "id": "service",
      "name": "Service contract",
      "tasks": [
        {
          "id": "task_00000000-0000-4000-8000-000000000003",
          "name": "Expose repository health",
          "dependencies": {},
          "acceptance_checks": [
            {
              "id": "status_response",
              "description": "Status returns the enrolled repository health"
            }
          ]
        }
      ]
    }
  ]
}
```

IDs use the prefixed UUIDs returned by the CLI/API. Dependency objects map a task
ID to `captured`, `development_available`, or `target_published`. Phase and check
keys use 1–64 lowercase letters, digits, underscores, or hyphens. Unknown fields,
empty phases, missing acceptance checks, duplicate IDs, unknown dependencies, and
cycles are rejected before any write occurs.

The `sealed` flag records whether planning is complete. Both drafts and sealed
plans retain full revision history; later execution commands will require a sealed
revision. Executable validation commands are deliberately not accepted from this
document—they are configured through the trusted repository profile.
