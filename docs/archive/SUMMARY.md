# ION Project Summary

> Generated from code analysis on 2025-07-24

---

## 1. Cargo.toml �� Dependencies

### Workspace Members
- `todo-plugin`
- `plan-plugin`
- `ion-provider` (local path dependency)

### Main Dependencies (24 crates)

| Crate | Version | Features / Notes |
|-------|---------|------------------|
| `tokio` | 1 | `full` |
| `tokio-util` | 0.7 | `rt` |
| `serde` | 1 | `derive` |
| `serde_json` | 1 | ��� |
| `rusqlite` | 0.31 | `bundled` (SQLite database) |
| `async-trait` | 0.1 | — |
| `thiserror` | 2 | — |
| `tracing` | 0.1 | — |
| `tracing-subscriber` | 0.3 | `env-filter` |
| `uuid` | 1 | `v4`, `serde` |
| `reqwest` | 0.12 | `json`, `rustls-tls`, `stream` (no default features) |
| `futures-util` | 0.3 | — |
| `ion-provider` | path | Local LLM provider abstraction crate |
| `clap` | 4 | `derive` (CLI argument parsing) |
| `axum` | 0.7 | `macros` (HTTP server) |
| `tower-http` | 0.6 | `cors` |
| `jsonschema` | 0.46.8 | JSON Schema validation |
| `wasmtime` | 44.0.3 | WebAssembly runtime (extensions) |
| `serde_yaml` | 0.9.34 | YAML serialization |
| `which` | 8.0.4 | Locating executables |
| `base64` | 0.22 | Base64 encoding |
| `similar` | 2 | Text diffing |
| `rmcp` | 1 | MCP client (child-process, streamable HTTP) |
| `zstd` | 0.13 | Zstd compression |

### Binary Targets (6)
| Binary Name | Source Path |
|-------------|-------------|
| `ion` | `src/bin/ion.rs` |
| `demo` | `src/bin/demo.rs` |
| `mock-worker` | `src/bin/mock_worker.rs` |
| `agent-demo` | `src/bin/agent_demo.rs` |
| `manager-test` | `src/bin/manager_test.rs` |
| `ion-worker` | `src/bin/ion_worker.rs` |

### Profile Settings
- **dev**: `opt-level=1`, debug symbols on
- **release**: `opt-level=3`, strip symbols

### `ion-provider` (sub-crate) Dependencies
- tokio (1, full), serde (1, derive), serde_json (1), async-trait (0.1), tracing (0.1), reqwest (0.12, json+rustls-tls+stream), futures-util (0.3), thiserror (2), regex (1), tokio-util (0.7, rt)

---

## 2. Source Code Structure

### `src/lib.rs` – Public Module Exports (40 modules)

| Module | Description (inferred) |
|--------|----------------------|
| `agent` | Agent loop, tools, bash, compaction, memory, extensions |
| `agent_config` | Agent configuration |
| `auth` | Authentication |
| `backend_registry` | Backend provider registry |
| `command_guard` | Command allow/block listing |
| `config` | Configuration management |
| `error` | Error types (`IonError`) |
| `event` / `event_bus` | Event system |
| `export` | Export functionality |
| `global_memory` / `global_memory_ext` | Global/shared memory |
| `ids` | Session/Task ID generation |
| `types` | Core types (`PoolOptions`, `Task`, `TaskPayload`, `TaskResult`, etc.) |
| `manager` | Agent manager |
| `wasm_extension` | WebAssembly extension runtime |
| `rpc` | RPC layer |
| `runtime` | Async runtime management |
| `pool` | Worker pool |
| `queue` | Task queue |
| `session` / `session_index` / `session_jsonl` / `session_tree` | Session persistence & indexing |
| `worker` / `worker_api` / `worker_registry` | Worker abstraction |
| `kernel` | Kernel operations |
| `paths` | Path management |
| `storage_context` | Storage context |
| `retry` | Retry logic |
| `workflow` | Workflow engine |
| `file_snapshot` / `file_time_guard` | File snapshot & time guard |
| `mcp` | Model Context Protocol integration |
| `hooks` | Hook system |
| `rules_engine` | Rules engine |
| `context_reclaimer` | Context reclamation |

### `src/agent/mod.rs` – Agent Sub-Modules (13)
- `agent_loop` �� Main agent orchestration loop
- `bash` – Bash execution tool
- `compact` – Session compaction (context window management)
- `context_index` – Context indexing
- `error` – Agent-specific errors
- `extension` – Extension framework
- `memory` – Agent memory
- `messages` – Re-exports `ion_provider::types::*`
- `permission_extension` – Permission-based extension
- `plan_extension` – Planning extension
- `provider` – Provider abstraction
- `tool` – Tool system (ReadTool, GrepTool, FindTool, LsTool, BashTool, WriteTool, EditTool, CalculatorTool, EchoTool, GitStatusTool, GitDiffTool, GitLogTool, GitAddTool, GitCommitTool, GitBranchTool, ToolRegistry)
- `workflow_extension` – Workflow extension

---

## 3. Main Binary (`src/bin/ion.rs`)

- **~229 KB** (5,291 lines)
- CLI entry point using `clap` with extensive argument support:
  - Provider selection (`--provider`, `--base-url`, `--api-key`, `--model`, `--models`)
  - Session management (`--resume`, `--continue`, `--no-session`, `--session-id`, `--session-dir`, `--fork`, `--branch`, `--checkout`, `--rollback`, `--restore-code`)
  - Tools control (`--tools`, `--exclude-tools`, `--no-builtin-tools`)
  - Prompt management (`--prompt`, `--system-prompt`, `--append-system-prompt`)
  - Extensions & skills (`--extension`, `--no-extensions`, `--skill`, `--no-skills`, `--prompt-template`, `--theme`)
  - Output modes (`text`, `json`, `rpc`)
  - Subcommands: `run`, `config`, `submit`, `serve`, `help`
- Builds environment info string (unix time, working directory, git branch, recent commits, uncommitted changes)

---

## 4. `src/utils.rs` – Not Found

This file does **not exist** in the project. There is no `utils.rs` anywhere in the source tree.

---

## 5. `src/models.rs` – Not Found

This file does **not exist** in the project. Model-related types live in:
- `ion-provider/src/types.rs` – LLM provider types (content blocks, tool definitions, model capabilities, streaming, usage stats)
- `src/types.rs` – ION core types (pool, task, session state)
- `src/agent/provider.rs` – Agent provider abstraction

---

## 6. Key Architectural Highlights

1. **Dual-crate structure**: `ion` (CLI + agent orchestration) + `ion-provider` (LLM API abstraction).
2. **Agent loop** with tool calling, session compaction, memory, and extension system.
3. **WebAssembly extensions** via `wasmtime` for sandboxed plugin execution.
4. **Model Context Protocol (MCP)** support via `rmcp`.
5. **Session persistence** using SQLite (`rusqlite`) with JSONL index, session tree, and file snapshots.
6. **Provider-agnostic**: supports OpenAI-compatible APIs (OpenAI, Anthropic, DeepSeek, etc.) via `ion-provider`.
7. **Async-first** with `tokio`, `axum` HTTP server for serving mode.
8. **Tool system** with 17+ built-in tools including git operations, bash, filesystem, calculator.

---

## File Count Summary

| Location | Files |
|----------|-------|
| `src/` | 40 `.rs` modules |
| `src/agent/` | 13 modules |
| `src/bin/` | 6 entry points |
| `src/mcp/` | MCP integration |
| `src/pool/` | Worker pool |
| `src/hooks/` | Hook system |
| `src/file_snapshot/` | File snapshot |
| `src/worker/` | Worker implementation |
| `ion-provider/src/` | Provider abstractions |
