# ION — AI Agent Orchestration Platform

> A self-evolving Rust implementation aligned with **pi** (pi-coding-agent).

![Rust](https://img.shields.io/badge/Rust-1.85%2B-ed2024?logo=rust)
![Edition](https://img.shields.io/badge/Edition-2024-orange)
![Tests](https://img.shields.io/badge/tests-490%2B-brightgreen)
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

```bash
# Build the two binaries
cargo build --bin ion --bin ion-worker

# Configure your provider API key
ion config set api-key "sk-xxx"

# Run a one-shot task
ion "hello"
```

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
