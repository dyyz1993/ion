---
name: scheduler
description: Schedule monitor-based automations. Use this skill when the user wants to set up recurring checks (e.g., "monitor GitHub issues every 5 minutes", "scan logs every minute", "check process every 30 seconds"). Produces valid .ion/monitors/*.json configs and validates them before install.
---

# Scheduler Skill — Monitor Configuration Generator

This skill gives the current agent the ability to **generate, validate, and install** monitor configurations (`.ion/monitors/*.json`). Any agent (build, coordinator, developer, reviewer, etc.) can load this skill to handle scheduling requests.

## When to Use

Load this skill when the user (or another agent) asks for any of:
- "Monitor X every N minutes" / "Check X periodically"
- "Set up a recurring task" / "Schedule a scan"
- "Watch for new GitHub issues / PRs / alerts"
- "Notify me when X happens"
- "Auto-handle Y when it appears"

If the request involves periodic checking + conditional action, this skill applies.

## ⚠️ Critical Rules (violating = failure)

1. **First action must be `read .ion/monitors/`** to avoid duplicate names
2. **After writing the .json, MUST call `extension_rpc monitor validate`** to verify
3. **After validate passes, MUST call `extension_rpc monitor test`** (dry-run) to verify the script actually runs and produces expected output
4. **Validate/test failures must be fixed and retried** — do NOT install with warnings
5. **Do NOT decide `mode`/`trigger_mode` unilaterally** — use the decision trees below; ask the user when ambiguous
6. **`prompt_template` MUST contain `{output}` placeholder** — otherwise downstream agent has no context

---

## Workflow (8 steps)

### Step 1: Read existing monitors

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}'
```

Also check the directory directly:
```bash
ls .ion/monitors/ 2>/dev/null
```

### Step 2: Clarify ambiguous requests

If the user's description is missing key info, ask:

| User says | Ask |
|-----------|-----|
| "monitor X" | How often? (suggest 60s / 300s / 3600s based on criticality) |
| "monitor GitHub issues" | Which repo? Which label filter? |
| "alert me when X" | How should I alert? (auto-spawn worker / channel notify / event only) |
| "scan X" | What's the trigger condition? (non-empty stdout = trigger) |

**Never guess.** If unclear, ask first.

### Step 3: Choose `mode` (concurrency policy) using this decision tree

```
What is being monitored?
├─ External state (GitHub issues / RSS / log scans)
│   └─ Previous task may not be done yet
│       └─ → serial_skip (default recommended)
│
├─ Periodic health check (CPU / disk / process alive)
│   └─ Each check is independent and quick
│       └─ → concurrent + max_concurrent=1
│
└─ Event stream dedup (webhook / message queue)
    └─ Cannot lose tasks, but can be slow
        └─ → serial_queue
```

| mode | Behavior | When to use |
|------|----------|-------------|
| `serial_skip` (default) | Previous worker still running → skip this tick | Heavy tasks, high frequency, no buildup |
| `serial_queue` | Busy → enqueue, replay when idle | Cannot lose tasks, latency tolerated |
| `concurrent` | Always spawn, up to N parallel | Light tasks, independent, parallelizable |

### Step 4: Choose `trigger_mode` (consumer routing) using this decision tree

```
What should happen after trigger?
├─ Fully automatic (unattended)
│   └─ → auto_spawn (default)
│
├─ Hand off to already-running coordinator
│   └─ → channel_notify
│
└─ Just notify, human decides
    └─ → event_only
```

| trigger_mode | Behavior | When to use |
|--------------|----------|-------------|
| `auto_spawn` (default) | Spawn `<agent>` worker directly | Full automation, CI/self-heal |
| `channel_notify` | Push to `main` channel for subscribers | Reuse existing workers, team coordination |
| `event_only` | Emit event only, no spawn | Manual takeover, debugging, audit |

### Step 5: Write the monitor.json (use this template)

```json
{
  "name": "<unique-id>",
  "interval_secs": 300,
  "script": "<bash one-liner: exit 0 + non-empty stdout = trigger>",
  "agent": "<registered agent name>",
  "prompt_template": "<MUST contain {output} placeholder>",
  "enabled": true,
  "mode": "serial_skip",
  "trigger_mode": "auto_spawn",
  "max_concurrent": 3,
  "cooldown_secs": 60
}
```

#### Field constraints (must obey)

| Field | Constraint | Failure mode |
|-------|-----------|--------------|
| `name` | regex `^[a-zA-Z0-9_-]{1,32}$` | validate rejects (path traversal guard) |
| `interval_secs` | 1-86400 (seconds) | 0 = busy loop; >86400 = rejected |
| `script` | non-empty, `bash -n` passes | validate rejects |
| `agent` | registered agent name | first trigger errors + auto-disable |
| `prompt_template` | MUST contain `{output}` | validate rejects |
| `mode` | serial_skip / serial_queue / concurrent | defaults to serial_skip |
| `trigger_mode` | auto_spawn / channel_notify / event_only | defaults to auto_spawn |
| `max_concurrent` | >= 1 (concurrent mode only) | defaults to 3 |
| `cooldown_secs` | >= 0 | defaults to 60 |

#### Script contract

- **exit=0 + stdout non-empty** = TRIGGER (this is the contract)
- **exit=0 + stdout empty** = no event (most common case)
- **exit≠0** = script error (5 consecutive failures → auto-disable)
- **stdout is for downstream agent**: keep concise, structured (JSON / table)
- **stderr does not affect trigger decision**, but is logged

Anti-patterns:

```bash
# ❌ Bad: always triggers (meaningless)
echo "always"

# ❌ Bad: silent failure (gh missing → empty stdout → no trigger, but issue exists)
gh issue list 2>/dev/null

# ✅ Good: explicit error to stderr on failure, data to stdout on success
gh issue list --repo X --json number 2>&1 | head -1 | grep -q '^[' && gh issue list --repo X --json number || echo ""
```

### Step 6: Validate (mandatory)

```bash
ion rpc --method extension_rpc --params '{
  "extension": "monitor",
  "method": "validate",
  "args": { ... your def ... }
}'
```

Expected success:
```json
{"success": true, "data": {"valid": true, "warnings": []}}
```

Expected failure:
```json
{"success": true, "data": {"valid": false, "errors": ["..."]}}
```

**On failure**: fix each error in the list, then re-validate. Loop until passes.

### Step 7: Dry-run (mandatory)

```bash
ion rpc --method extension_rpc --params '{
  "extension": "monitor",
  "method": "test",
  "args": {
    "script": "<your script>",
    "prompt_template": "<your prompt_template>"
  }
}'
```

Expected success:
```json
{
  "success": true,
  "data": {
    "valid": true,
    "script_exit_ok": true,
    "script_stdout": "<actual output>",
    "script_duration_ms": 12,
    "would_trigger": true,
    "rendered_prompt": "<prompt with {output} replaced>"
  }
}
```

**Check**:
- ✅ `script_exit_ok=true` (script runs)
- ✅ `would_trigger=true` (stdout non-empty)
- ✅ `rendered_prompt` reads naturally
- ⚠️ `script_duration_ms > 5000` → script too slow, consider optimizing

**On failure**:
- `script_exit_ok=false` → fix script per stderr
- `would_trigger=false` → script logic error (gh missing / grep no match), consider mock data or different command
- `rendered_prompt` garbled → fix prompt_template

### Step 8: Install (after validate + test pass)

```bash
ion rpc --method extension_rpc --params '{
  "extension": "monitor",
  "method": "add",
  "args": { ... your def ... }
}'
```

Expected:
```json
{"success": true, "data": {"added": "<name>", "validated": true, "file": ".ion/monitors/<name>.json"}}
```

### Step 9: Report installation

After successful install, tell the user:

```
✅ Monitor installed:

  Name: <name>
  Interval: every N seconds
  Script: <one-line summary>
  Trigger condition: <what non-empty stdout means>
  Handling: <mode + trigger_mode summary>
  File: .ion/monitors/<name>.json

Activate via:
  ion serve            # scene 3 (long-running)
  ion --host "..."     # scene 2 (with monitor auto-running)

Check status:
  ion rpc --method extension_rpc --params '{"extension":"monitor","method":"status"}'

Watch events live:
  ion subscribe
```

---

## Examples

### Example 1: GitHub issue monitoring (serial_skip + auto_spawn)

User: *"Monitor https://github.com/dyyz1993/ion for new bug issues every 5 minutes"*

Output:

```json
{
  "name": "github-issues",
  "interval_secs": 300,
  "script": "gh issue list --repo dyyz1993/ion --state open --label bug --json number,title 2>/dev/null",
  "agent": "developer",
  "prompt_template": "GitHub bug issues:\n{output}\n\nReview each (use gh issue view <number>)",
  "enabled": true,
  "mode": "serial_skip",
  "trigger_mode": "auto_spawn"
}
```

**Why serial_skip**: issue handling can be slow (read code, edit, test); 5 minutes may not be enough; do not pile up.

### Example 2: Log error scan (serial_queue + channel_notify)

User: *"Monitor /var/log/myapp.log for ERROR, route to coordinator, do not lose"*

```json
{
  "name": "error-log-scan",
  "interval_secs": 60,
  "script": "grep -E 'ERROR|panic' /var/log/myapp.log 2>/dev/null | tail -10",
  "agent": "coordinator",
  "prompt_template": "Log errors:\n{output}\n\nCoordinate investigation",
  "enabled": true,
  "mode": "serial_queue",
  "trigger_mode": "channel_notify"
}
```

**Why serial_queue**: user said "do not lose" → enqueue for idle worker.
**Why channel_notify**: hand to already-running coordinator, don't spawn new.

### Example 3: Process alive check (concurrent + event_only)

User: *"Check every 30s if critical-service is alive, alert me on death"*

```json
{
  "name": "process-alive",
  "interval_secs": 30,
  "script": "pgrep -f 'critical-service' > /dev/null 2>&1 || echo 'CRITICAL_SERVICE_DOWN'",
  "agent": "user",
  "prompt_template": "Critical process issue: {output}",
  "enabled": true,
  "mode": "concurrent",
  "max_concurrent": 1,
  "trigger_mode": "event_only"
}
```

**Why concurrent + max_concurrent=1**: independent checks, but cap notification storms.
**Why event_only**: user said "alert me", not "auto-handle".

### Example 4: Disk usage scan (concurrent + auto_spawn)

User: *"Hourly disk check, above 80% have maintainer clean up"*

```json
{
  "name": "disk-usage",
  "interval_secs": 3600,
  "script": "df -h | awk 'NR>1 && $5+0 > 80 {print $5\" \"$6}'",
  "agent": "maintainer",
  "prompt_template": "Disk warnings:\n{output}\n\nClean up affected partitions",
  "enabled": true,
  "mode": "concurrent",
  "max_concurrent": 3,
  "trigger_mode": "auto_spawn"
}
```

**Why concurrent**: multiple partitions may exceed simultaneously, handle independently.

---

## Self-check (before installing)

- [ ] `name` is a valid identifier (`^[a-zA-Z0-9_-]{1,32}$`), no duplicate
- [ ] `interval_secs` in 1-86400, matches user's "how often"
- [ ] `script` non-empty, `bash -n` passes
- [ ] `script` failure produces stderr (not silent)
- [ ] `agent` is a registered agent
- [ ] `prompt_template` contains `{output}`
- [ ] `prompt_template` reads naturally (downstream agent can understand)
- [ ] `mode` matches decision tree
- [ ] `trigger_mode` matches decision tree
- [ ] `max_concurrent` reasonable (for concurrent mode)
- [ ] `validate` returned `valid: true`
- [ ] `test` returned `would_trigger: true` (or logic confirmed in dry-run)
- [ ] `add` returned `validated: true`

---

## Failure modes (do NOT make these mistakes)

### Error 1: skip validate, install directly

```
❌ Write .json → add directly
✅ MUST validate → test → add (three steps)
```

### Error 2: pick mode unilaterally

```
❌ "monitor X" → assume serial_skip
✅ If user didn't specify, use decision tree + explain "I picked serial_skip because..."
```

### Error 3: silent script failure

```bash
❌ gh issue list 2>/dev/null       # gh missing → silent empty → no trigger, but issues exist
✅ gh issue list 2>&1 | head -1 | grep -q '^[' && gh issue list || echo "GH_ERROR"
```

### Error 4: prompt_template missing placeholder

```
❌ "Handle new issues"          # downstream agent doesn't know what issues
✅ "New issues:\n{output}\n..."  # {output} is replaced with script output
```

### Error 5: invalid name

```
❌ "github issues"     # contains space
❌ "监控/日志"          # non-ASCII
❌ "../../../etc/xxx"  # path traversal
✅ "github-issues"
✅ "error-log-scan"
```

---

## Reference

- Design doc: `docs/design/MONITOR_EXTENSION.md`
- CLI test cases: `docs/testing/MONITOR_CLI_TEST.md`
- Implementation: `src/monitor_extension.rs`
- Events emitted (subscribe-visible): `monitor_triggered`, `monitor_skipped`, `monitor_queued`, `monitor_spawned`, `monitor_throttled`, `monitor_cooldown`, `monitor_script_failed`, `monitor_event_only`, `monitor_channel_notify`, `monitor_no_subscriber`, `monitor_queue_overflow`
