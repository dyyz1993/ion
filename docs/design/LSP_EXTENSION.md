# LSP Extension Design

## Overview

LSP Extension provides compiler diagnostics feedback to the LLM after code changes. Uses project-specific linters (cargo clippy, tsc, ruff, go vet) instead of full LSP. Runs as a worker-level extension, scoped to each worker's project directory.

## How it Works

1. Agent calls `write` or `edit` tool → `on_tool_execution_end` sets `dirty=true` and starts async background check
2. Next LLM call → `on_context` picks up background results, injects `<diagnostics>` XML block
3. LLM sees errors → fixes code → writes again → cycle repeats until clean

## Key Structs

### Diagnostic
Represents a single compiler diagnostic.
- `file`, `line`, `column`: Location
- `severity`: "error" | "warning"
- `message`, `code`: Error details
- `suggestion`: Machine-applicable fix (Rust only)

### LspExtension
Main extension state (worker-scoped).
- `diagnostics`: Shared diagnostics vector
- `dirty`: Files changed flag
- `has_errors`: Last check error state
- `project_root`: Detected project root
- `changed_files`: Incremental check priority
- `check_count`: Loop detection (max 10/session)
- `last_check_time`: Cooldown (min 3s)
- `bg_check_running` / `bg_check_ready`: Async check flags

## Diagnostics Flow

### on_tool_execution_end
- Triggers on write/edit tool completion
- Sets `dirty=true`, tracks changed file
- Spawns tokio task for async linter run (non-blocking)
- Background check updates diagnostics + sets `bg_check_ready=true`

### on_context
- Only injects if `bg_check_ready=true` (non-blocking)
- Dedup: skips if identical to last injected diagnostics
- Compresses old diagnostics (keep 2 recent, rest → summaries)
- Formats as `<diagnostics>` XML with all errors + first 5 warnings

## Agent Loop Integration

The extension hooks into two key lifecycle points:

1. **Post-tool execution** (`on_tool_execution_end`): Flags workspace as dirty and starts async linter check
2. **Context building** (`on_context`): Injects fresh diagnostics as CustomMessage if available

This creates a feedback loop: write → check → diagnose → fix → repeat.

## Supported Languages

Auto-detects by project marker files:
- **Rust**: `Cargo.toml` → `cargo clippy --message-format=json`
- **TypeScript**: `package.json` / `tsconfig.json` → `tsc --noEmit`
- **Python**: `pyproject.toml` / `setup.py` / `requirements.txt` → `ruff check` or `py_compile`
- **Go**: `go.mod` → `go vet ./...`
- **HTML**: `index.html` or `*.html` files → custom tag validation

## Config Options

```json
{ "extensions": { "lsp": { "enabled": true } } }
```

Environment variables:
- `ION_LSP_TIMEOUT`: Max check duration (default: 120s)

Metrics logged to `~/.ion/agent/lsp-metrics.jsonl` for self-evolution analysis.