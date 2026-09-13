# Schedule policy format

A schedule policy is a portable JSON document validated by its own domain type
before the daemon activates it. There is no partial or best-effort validation:
malformed fields, an empty day set, overlapping windows, and a daily maximum the
windows cannot actually hold are all rejected before anything is stored.

```json
{
  "timezone": "America/New_York",
  "allowed_days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
  "windows": [
    {"start": {"hour": 9, "minute": 0}, "end": {"hour": 17, "minute": 0}}
  ],
  "daily_releases": {"minimum": 1, "maximum": 3},
  "minimum_spacing_minutes": 45,
  "missed_window_behavior": {"kind": "reschedule_forward"}
}
```

`timezone` is a named IANA zone, validated against the bundled time-zone
database — this is what keeps daylight-saving transitions and civil-time
arithmetic correct rather than approximated. `allowed_days` and `windows` are
in that zone's local time, not UTC; at least one of each is required.

`windows` are half-open (`start` inclusive, `end` exclusive) and must not
overlap once sorted. `daily_releases.maximum` cannot exceed how many slots the
windows can actually hold at `minimum_spacing_minutes` apart — a policy that
claims capacity its own windows do not have is rejected, not silently
truncated.

`missed_window_behavior` is one of:

```json
{"kind": "reschedule_forward"}
{"kind": "catch_up", "max_releases": 5}
```

`reschedule_forward` is the default-safe choice: an overdue slot moves to the
next eligible window rather than backdating a commit. `catch_up` permits a
deliberately bounded burst after an offline gap; unbounded catch-up is never
allowed.

Activating a policy through `schedule set-policy` appends a new, immutable
revision — it never edits one in place. Revisions apply only to future
selections; a release unit that already has a durable slot keeps the policy
revision that selected it until an explicit recalculation invalidates it.
