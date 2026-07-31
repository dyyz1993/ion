# File Snapshot Usage Guide

## What File Snapshot Does

File Snapshot tracks every file change made by agents during a session. It provides:

- Precise diffs for write/edit operations
- Change history queries per file
- Per-file approval workflow (review pending changes)
- Rollback capabilities with `--restore-code`
- Dual-path capture: tool-level (write/edit) + directory scan (bash)

Storage is content-addressed with deduplication, keeping usage under 100MB via automatic GC.

---

## Enable in config.json

Add to your session config:

```json
{
  "extensions": {
    "file_snapshot": {
      "enabled": true,
      "max_store_mb": 100
    }
  }
}
```

---

## RPC Commands

### get_modified_files
List changed files within a turn range.

```bash
ion rpc --session <sid> --method get_modified_files \
  --params '{"fromTurn": 1, "toTurn": 5}'
```

Response includes `path`, `status` (added/modified/deleted), `source` (tool_write/tool_edit/turn_scan), `turnId`.

### get_file_diff
Get unified diff for a specific file.

```bash
ion rpc --session <sid> --method get_file_diff \
  --params '{"filePath": "src/main.rs", "fromTurn": 1, "toTurn": 5}'
```

Returns unified diff with `beforeHash` and `afterHash`.

### get_batch_diffs
Get diffs for all modified files in a range.

```bash
ion rpc --session <sid> --method get_batch_diffs \
  --params '{"fromTurn": 1, "toTurn": 5}'
```

Aggregates diffs with line count summary.

### get_file_history
Show complete change timeline for a file.

```bash
ion rpc --session <sid> --method get_file_history \
  --params '{"filePath": "src/main.rs"}'
```

Returns list of changes with turnId, action, and hash.

### review_pending
List files awaiting approval.

```bash
ion rpc --session <sid> --method review_pending
```

Shows pending changes with diffs for manual review.

---

## restore_files + --restore-code

Restore code to a previous turn state:

```bash
# Restore only code (independent of messages)
ion rpc --session <sid> --method restore_files \
  --params '{"toTurn": 5}'

# Rollback messages AND restore code together
ion --resume <sid> --rollback msg_005 --restore-code
```

How it works:
1. Collects all snapshots after target turn
2. Reverts each file to its pre-change state (deletes if it didn't exist before)
3. Creates a restore_point snapshot for undo capability

---

## Approval Workflow

Per-file approval lets you review and control changes:

1. **Review pending**: `review_pending` lists all unapproved changes with diffs
2. **Approve single**: `review_approve` accepts specific file changes, anchoring to baseline
3. **Reject single**: `review_reject` rejects and auto-rolls back that file
4. **Bulk actions**: `review_approve_all` / `review_reject_all`
5. **Query status**: `review_approvals` checks current approval state

Example:
```bash
# See what needs approval
ion rpc --session <sid> --method review_pending

# Approve a specific change
ion rpc --session <sid> --method review_approve \
  --params '{"filePath": "src/main.rs", "toTurn": 5}'

# Reject a change (auto-rollback)
ion rpc --session <sid> --method review_reject \
  --params '{"filePath": "src/main.rs", "toTurn": 5}'
```

---

## CLI Examples

```bash
# Check what changed in recent turns
ion rpc --session <sid> --method get_modified_files

# See diff for a specific file
ion rpc --session <sid> --method get_file_diff \
  --params '{"filePath": "Cargo.toml"}'

# Review full history of a file
ion rpc --session <sid> --method get_file_history \
  --params '{"filePath": "src/main.rs"}'

# Rollback to message + restore code
ion --resume <sid> --rollback msg_010 --restore-code

# Review pending approvals
ion rpc --session <sid> --method review_pending
```

---

## Notes

- Git-ignored text files (.env, configs) ARE tracked (valuable changes)
- Build directories (target/, node_modules/) are skipped (DEFAULT_IGNORE)
- Files > 1MB are skipped (truncation)
- GC runs on session start (async, non-blocking)
- Worktrees share object store but have isolated change records