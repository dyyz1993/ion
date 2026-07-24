# ION — AI Agent Orchestration Platform

> A self-evolving Rust implementation aligned with **pi** (pi-coding-agent).

![Rust](https://img.shields.io/badge/Rust-1.85%2B-ed2024?logo=rust)
![Edition](https://img.shields.io/badge/Edition-2024-orange)
![Tests](https://img.shields.io/badge/tests-777%2B-brightgreen)
![WASM](https://img.shields.io/badge/WASM_extensions-3-blue)
![License](https://img.shields.io/badge/license-MIT-blue)

---

ION is a Rust implementation of an AI Agent orchestration platform that aligns with
**pi** (pi-coding-agent). It supports multi-agent orchestration (`spawn_worker` / `peer`
/ `child`), WASM extensions, the MCP protocol, session-tree branching, and
**self-evolution** via an A→B architecture — where a coordinator agent (A) directs a
developer agent (B) that writes code inside an isolated container.

---

## Execution Scenarios

### Scenario 1 — Quick Execution

```bash
ion "summarize this repo"
```

Direct spawn. No host process — the CLI runs a single agent turn and exits.

```
┌────────┐   spawn    ┌────────┐
│  ion   │ ─────────▶ �� Worker │ ──▶ done ──▶ exit
└────────┘            └────────┘
```

### Scenario 2 — Quick Orchestration

```bash
ion --host "refactor the auth module and add tests"
```

A temporary **host** is spawned with an event pump, enabling multi-agent coordination
for the duration of the task. The host tears down on completion.

```
┌────────┐  spawn host   ┌──────────────┐  spawn_worker   ┌────────┐
│  ion   │ ─────────────▶│  Host + Pump │ ───────────────▶│ Worker │
└────────┘               └──────────────┘                 └────────┘
                                │ await / resume / kill
                                ▼
```

### Scenario 3 — Persistent Service

```bash
ion serve              # always-on host via Unix socket
ion "do something"     # any client connects to the running host
```

An always-on host listening on a Unix domain socket. Multiple CLI invocations connect
to the same long-lived orchestration core.

```
┌──────────────────────────────────────────┐
│  ion serve  (persistent host, Unix sock) │
│                                          │
│   ┌─────────┐  ┌─────────┐  ┌─────────┐  │
│   │ Worker  │  │ Worker  │  │ Worker  │  │
│   └─────────┘  └─────────┘  └─────────┘  │
└──────────────────────────────────────────┘
        ▲                ▲                ▲
        └──── clients ───┴──── connect ───┘
```

---

## Quick Start

### Prerequisites

- **Rust** 1.85+ (2024 edition)
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source $HOME/.cargo/env
  ```

### Build

```bash
git clone https://github.com/dyyz1993/ion.git
cd ion

# Build both binaries (ion CLI + ion-worker subprocess)
cargo build --bin ion --bin ion-worker

# Verify installation
./target/debug/ion --version
```

### Configure

```bash
# Set your API key (stored in ~/.ion/auth.json, permissions 600)
ion config set api-key "sk-xxx"

# Or create ~/.ion/config.json manually:
# {
#   "default_provider": "zai",
#   "default_model": "glm-5.2",
#   "providers": {
#     "zai": {
#       "name": "zai",
#       "api": "openai-completions",
#       "base_url": "https://your-api-endpoint/v1",
#       "api_key": "your-token"
#     }
#   }
# }
```

### Run

```bash
# One-shot task (Scenario 1: quick execution)
ion "summarize this repo"

# Multi-agent orchestration (Scenario 2: quick host)
ion --host --agent coordinator "add a method to src/auth.rs"

# Persistent service (Scenario 3: daemon)
ion serve
```

### Supported Providers

ION supports 10 API protocols (aligned with pi):
- OpenAI Completions / Responses / Codex
- Anthropic Messages
- Azure OpenAI Responses
- Google Generative AI / Vertex
- Mistral Conversations
- Cloudflare Workers AI
- Amazon Bedrock

Use any provider by configuring ~/.ion/config.json.

---

## WASM Extensions

ION supports hot-pluggable WASM extensions. Drop a `.wasm` file into `~/.ion/agent/extensions/` and it's auto-discovered — no recompilation needed.

### Install Pre-built Extensions

```bash
# Rules Engine — inject project rules into system prompt based on file globs
cp extensions/rules-engine/rules_engine.wasm ~/.ion/agent/extensions/

# File Time Guard — block writes to stale files (prevents clobbering user edits)
cp extensions/file-time-guard/file_time_guard.wasm ~/.ion/agent/extensions/

# Session Supervisor — auto-scan for TODO/FIXME after agent finishes
cp extensions/session-supervisor/session_supervisor.wasm ~/.ion/agent/extensions/
```

### Use Rules Engine

Create `.ion/rules/rust.md` in your project:
```
---
applyTo: "**/*.rs"
---

- Use snake_case for function names
- Add doc comments for public functions
- Run `cargo fmt` before committing
```

The rules are automatically injected into the agent's system prompt when it works on `.rs` files.

### Write Your Own Extension

WASM extensions have access to **27 host functions** and **36 lifecycle hooks**:

| Category | Host Functions |
|----------|---------------|
| File system | `host_read_file`, `host_write_file`, `host_list_dir`, `host_path_exists`, `host_glob` |
| Data storage | `host_read/write/delete/list_{global,project,project_local,session}_data` (16 functions) |
| Agent state | `host_get_token_count`, `host_get_messages`, `host_get_state`, `host_steer`, `host_llm_call` |
| Communication | `host_send_message`, `host_channel_send`, `host_create_worker` |
| UI | `host_ui_ask`, `host_ui_confirm`, `host_ui_notif`, `host_ui_alert`, `host_ui_prompt` |
| Tools | `host_register_tool` |

See `extensions/rules-engine/src/lib.rs` for a complete example.

---

## Next Steps

### Try different agents
```bash
# Explore codebase (read-only)
ion --agent explore "analyze this repo"

# Code review
ion --agent reviewer "review src/auth.rs"

# Multi-agent orchestration
ion --host --agent coordinator "add a method to src/global_memory.rs"
```

### Export reports
```bash
ion --export /tmp/report.html
open /tmp/report.html
```

### Browse sessions
```bash
ion sessions              # list all sessions
ion history <session-id>  # view conversation history
```

### Learn more
- [CHANGELOG.md](CHANGELOG.md) — What's new in each version
- [CLI_USAGE.md](docs/guides/CLI_USAGE.md) — Full CLI reference
- [WORKFLOW.md](docs/guides/WORKFLOW.md) — Development workflows
- [CONTRIBUTING.md](CONTRIBUTING.md) — How to contribute

---

## Key Features

- **45+ CLI parameters** — fully aligned with pi's command surface.
- **Multi-agent orchestration** — `spawn_worker` (child/peer), `resume_worker`,
  `await_worker`, `kill_worker`, `channel_send`.
- **Self-evolution** — A→B architecture: A orchestrates, B writes code in an isolated
  container.
- **WASM extensions** — hot-pluggable modules with 31 lifecycle hooks.
- **MCP protocol support** — built on `rmcp` 1.x with a shared connection pool.
- **Session-tree branching** — fork, rollback, and a leaf pointer for nonlinear
  conversations.
- **HTML export** — rendered transcripts with tools panel, system prompt, and agent
  info.
- **490+ tests passing.**

---

## Self-Evolution (A→B)

ION can modify its own source code through a strict two-agent split:

```
  ┌───────────────────────┐         ┌───────────────────────────┐
  │   A — Coordinator     │  plan   │   B — Developer           │
  │   (host agent)        │ ──────▶ │   (isolated container)    │
  │                       │         │                           │
  │   • never writes code │ ◀────── │   • writes & tests code   │
  │   • reviews & merges  │  PR     │   • opens a pull request  │
  └───────────────────────┘         └───────────────────────────┘
                 │
                 ▼
       6 gate checks before merge
```

- **A** (coordinator) breaks down the goal, dispatches work, and reviews results — it
  never edits source files directly.
- **B** (developer) runs inside an isolated container, writes code, runs the test
  suite, and opens a PR.
- **6 gate checks** (compile, test, lint, format, doc, review) must all pass before A
  merges.

See [`docs/design/SELF_EVOLUTION.md`](./docs/design/SELF_EVOLUTION.md).

---

## Architecture Overview

```
                          ┌─────────────────────────────────────┐
                          │              ION Core                │
                          │                                     │
   Scenario 1 ──────────▶ │   AgentManager (actor)              │
   (direct spawn)         │     ├─ WorkerPool                   │
                          │     ├─ TaskQueue                    │
   Scenario 2 ──────────▶ │     ├─ Host + Event Pump            │
   (temp host)            │     ├─ Session Tree (fork/rollback) │
                          │     ├─ WASM Extension Host           │
   Scenario 3 ──────────▶ │     ├─ MCP Connection Pool           │
   (persistent serve)     │     └─ Self-Evolution (A→B)          │
                          │                                     │
                          │   Unix Socket  ◀── ion serve        │
                          └────────────────────────────────���────┘
```

---

## Documentation

- **[AGENTS.md](./AGENTS.md)** — Full project documentation (authoritative).
- **[docs/guides/CLI_USAGE.md](./docs/guides/CLI_USAGE.md)** — CLI usage guide.
- **[docs/design/](./docs/design/)** — Design documents (session tree, MCP,
  self-evolution, extensions, memory, and more).

---

## License

MIT
