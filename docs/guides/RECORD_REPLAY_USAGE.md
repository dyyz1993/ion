# Record/Replay Usage Guide

## What is Record/Replay?

Record/Replay lets you record LLM decisions from a real session and replay them offline.

During recording, every LLM response (including tool calls) is saved to disk.
During replay, those saved responses are played back in sequence, reproducing the exact decision flow without network or API costs.

**Important:** Only LLM decisions are replayed. Tools (read, edit, bash, etc.) execute for real, affecting your actual filesystem.

---

## Recording

Set the `ION_RECORD` environment variable with a unique ID:

```bash
# Record a session
ION_RECORD=fix-bug-2025-01-15 ion --model glm-4.6 "Fix the calc function"

# Multi-turn sessions are recorded as well
ION_RECORD=refactor-session ion --model glm-4.6 "Refactor utils"
```

Recording happens automatically:
- Responses saved to `~/.ion/recordings/<id>/trace.jsonl`
- Metadata in `~/.ion/recordings/<id>/meta.json`
- Recording persists when process exits

---

## Replaying

Use the recording ID with the replay provider:

```bash
# Replay without network or API keys
ion --model replay/fix-bug-2025-01-15 "Fix the calc function"
```

Replay behavior:
- Uses saved responses in original order (FIFO)
- Tools execute for real (permissions still apply)
- Error if recording ID not found or recording exhausted

---

## Managing Recordings

List and inspect recordings:

```bash
# List all recordings
ion recordings list

# Show recording details
ion recordings show fix-bug-2025-01-15

# Delete a recording
ion recordings delete fix-bug-2025-01-15
```

---

## Security

**Path Traversal Protection**

Recording IDs are strictly validated:
- Only alphanumeric, dot, dash, underscore allowed: `^[a-zA-Z0-9._-]{1,80}$`
- Path canonicalization ensures no directory traversal: `replay/../../etc/passwd` is rejected

**File Permissions**

- Recordings directory: `0700` (owner-only)
- Trace and meta files: `0600` (owner-only)

**Tool Execution During Replay**

Tools run for real with full security checks:
- Permissions and CommandGuard remain active
- Replay cannot bypass security systems
- Warning printed on replay start

**Best Practice:** Run replays in isolated worktrees or temporary directories, not production projects.

---

## CLI Examples

```bash
# Record a bug fix session
ION_RECORD=fix-read-parse ion --model glm-4.6 "Fix the read function"

# Replay it (no network, no API key needed)
ion --model replay/fix-read-parse "Fix the read function"

# Overwrite existing recording
ION_RECORD_OVERWRITE=1 ION_RECORD=fix-read-parse ion --model glm-4.6 "Test again"

# List recordings
ion recordings list
# Output: fix-read-parse | glm-4.6 | 3 responses | 2025-01-15T10:30:00Z