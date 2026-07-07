# ION — AI Agent Orchestration CLI (Rust)

A Rust implementation of an AI Agent orchestration CLI (对齐 pi),
built to study **lifetimes, ownership, async actors, and supervision** in Rust.

> 本文件是早期 Rust 学习阶段的 demo 说明，保留作历史记录。
> 当前项目的权威文档是 [AGENTS.md](./AGENTS.md)。

---

## Architecture

```
                 ┌──────────────────────────────────────┐
                 │            ManagerHandle              │  ← Clone, 'static, no lifetime params
                 │         (mpsc::Sender<Cmd>)           │
                 └────────────────┬─────────────────────┘
                                  │
                 ┌────────────────▼─────────────────────┐
                 │            AgentManager (actor)        │
                 │                                       │
                 │  ┌────────────┐  ┌──────────────────┐ │
                 │  │ WorkerPool │  │  TaskQueue        │ │
                 │  │ (actor)    │  │  (actor)          │ │
                 │  │            │  │                   │ │
                 │  │  ┌──────┐  │  │  VecDeque<Task>  │ │
                 │  │  │ W1   │←│←│←  mpsc recv       │ │
                 │  │  │ W2   │←│←│←                   │ │
                 │  │  │ WN   │  │  │                   │ │
                 │  │  └──────┘  │  └──────────────────┘ │
                 │  └────────────┘                        │
                 │  ┌────────────┐                        │
                 │  │ SessionStore│  ← Arc<RwLock<HashMap>>│
                 │  └────────────┘                        │
                 │  ┌────────────┐                        │
                 │  │  EventBus  │  ← tokio::broadcast    │
                 │  └────────────┘                        │
                 └────────────────────────────────────────┘
```

### Worker trait hierarchy

```
     trait Worker          ← Box<dyn Worker + Send>
        │
    ┌───┴──────────┐
    │              │
 StubWorker    ChildProcessWorker
 (in-process)   (spawns binary, JSONL stdin/stdout)
```

### State machines

**Worker lifecycle:** `Idle → Claimed → Connecting → Running → Completed → (Idle reuse | Dead)`
**Task lifecycle:**  `Queued → Running → (Completed | Failed[→Queued retry] | Cancelled)`

---

## Key Design: Ownership & Lifetimes

This is the most important section for the Rust study. Every design decision
below is made to **eliminate explicit lifetime parameters** across async boundaries.

### 1. Cross-channel data is always owned (`'static`)

| Pattern Used | Pattern Avoided | Why |
|---|---|---|
| `TaskId(Uuid)` (Copy) | `&'a str` | `TaskId` flows through mpsc channels and exists in spawned tasks. A `&'a str` would need the borrower (e.g. `Manager`) to outlive the spawned task, which is impossible to prove to the compiler. `Uuid` is Copy + Send + 'static → no lifetime. |
| `String` for text payloads | `&'a str` | Same reason — the spawned worker task must own the prompt text. |
| `oneshot::Sender<T>` | callback closures with `'a` | A `oneshot::Sender` is an owned handle to a one-shot channel. It can be sent into a spawned task without binding lifetimes. |

### 2. Actor pattern → no `&mut` shared state

Each component (`WorkerPool`, `TaskQueue`) is an **actor** — it owns its state
and processes commands via a `mpsc::Receiver`. External code communicates via
`mpsc::Sender<Cmd>` which is:
- `Clone` (many clients)
- `Send + 'static` (cross-task)
- **No lifetime parameter** (not tied to the actor's lifetime)

This avoids `Arc<Mutex<HashMap>>` for the hot path. Only `SessionStore` uses
`Arc<RwLock<…>>` because it's read-heavy.

### 3. `Worker` trait → `Box<dyn Worker + Send>`

The pool must hold heterogeneous workers (StubWorker for testing,
ChildProcessWorker for production). Generic `WorkerPool<W: Worker>` can only
hold one concrete type, so we use dynamic dispatch. This costs one vtable call
per method — negligible compared to JSON serialisation or subprocess I/O.

```rust
#[async_trait]
pub trait Worker: Send {
    async fn connect(&mut self) -> IonResult<()>;
    async fn prompt(&mut self, text: String) -> IonResult<TaskResult>;
    async fn steer(&mut self, msg: String) -> IonResult<()>;
    async fn state(&mut self) -> IonResult<SessionState>;
    async fn dispose(&mut self) -> IonResult<()>;
}
```

### 4. Cancellation via RAII

`ChildProcessWorker` calls `child.start_kill()` in its `Drop` impl. If the
worker task is cancelled (via `CancellationToken`), the `JoinHandle` is
dropped, which drops the task's future, which drops the `Worker` impl, which
drops the `Child` → subprocess is killed automatically. **No manual cleanup
needed.**

---

## Module Map

| File | Contents |
|---|---|
| `src/ids.rs` | `TaskId`, `WorkerId`, `SessionId` newtypes (Copy, 'static) |
| `src/types.rs` | `Task`, `TaskResult`, `TaskSnapshot`, `PoolOptions`, `PoolStats`, state enums |
| `src/event.rs` | `Event` enum + `EventBus` (broadcast channel wrapper) |
| `src/error.rs` | `IonError` (thiserror) + `IonResult<T>` |
| `src/worker/mod.rs` | `Worker` trait, `WorkerStatus` state machine, `WorkerHandle`, `WorkerCmd` |
| `src/worker/stub.rs` | `StubWorker` — in-process echo worker for testing |
| `src/worker/child.rs` | `ChildProcessWorker` — spawns subprocess, JSONL over stdio |
| `src/pool/mod.rs` | `WorkerPool` actor + `PoolHandle` — acquire/release/scaling/reaper |
| `src/pool/slot.rs` | `WorkerSlot` — metadata for one worker in the pool |
| `src/queue.rs` | `TaskQueue` actor — FIFO + retries (with configurable budget) |
| `src/session.rs` | `SessionStore` trait + `InMemorySessionStore` |
| `src/manager.rs` | `AgentManager` actor + `ManagerHandle` — top-level orchestrator |
| `src/bin/demo.rs` | Demo binary: submits 8 tasks, streams events |
| `src/bin/mock_worker.rs` | JSONL mock worker for `ChildProcessWorker` testing |

---

## Verification & Testing

### Quick commands

```bash
# Format
cargo fmt --check

# Lint (deny warnings)
cargo clippy --all-targets -- -D warnings

# All tests
cargo test

# Run demo
cargo run --bin demo

# Test with miri (catches UB, use nightly)
# rustup toolchain install nightly
# rustup +nightly component add miri
# cargo +nightly miri test --lib
```

### Test output (current: 29 tests, all green)

```
running 29 tests
test ids::tests::task_id_is_copy                ... ok
test event::tests::event_bus_publish_subscribe  ... ok
test types::tests::task_status_transitions_are_detected ... ok
test worker::tests::worker_status_exhaustive    ... ok
test pool::tests::pool_acquire_and_use          ... ok
test queue::tests::enqueue_dequeue              ... ok
test queue::tests::fail_triggers_retry          ... ok
test queue::tests::fail_exhausts_retries        ... ok
test session::tests::set_get_delete             ... ok
test manager::tests::submit_and_complete        ... ok
test manager::tests::submit_multiple_tasks      ... ok
test manager::tests::events_are_emitted         ... ok
... all 29 tests pass
```

### Demo output

```
ION Demo — Agent Manager Orchestration
Submitted 8 tasks…
  [submit] task_A submitted
  [submit] task_B submitted
  ...
  [start]  task_A → wkr wkr_1
  [done]   task_A success=true
  [start]  task_B → wkr wkr_1
  [done]   task_B success=true
  ...
=== Final Stats ===
Pool:  total_workers=1 idle=1
Queue: completed=8 failed=0 cancelled=0
```

### What's tested

| Layer | What | How |
|---|---|---|
| **State machines** | `WorkerStatus` and `TaskStatus` transitions | Exhaustive combinatorial test of every valid/invalid transition |
| **IDs** | `TaskId` uniqueness, Copy semantics | Unit tests |
| **EventBus** | Publish / subscribe, event formatting | Unit tests |
| **StubWorker** | Echo, failure modes | Unit tests |
| **WorkerPool** | Acquire/release/reuse, scale-to, max-workers enforcement | Actor tests with StubWorker |
| **TaskQueue** | Enqueue/dequeue/complete, retry up to N, cancel | Actor tests |
| **SessionStore** | CRUD, list | Unit tests |
| **AgentManager** | Submit → complete, multi-task, event streaming | End-to-end actor tests |
| **Demo** | 8 tasks with concurrent pool | Manual run |

### miri (if nightly installed)

For the `ids`, `types`, `event`, and `worker` modules which contain pure-data
logic, `miri` can detect undefined behaviour and certain lifetime violations:

```bash
cargo +nightly miri test --lib
```

---

## Dependencies

| Crate | Use |
|---|---|
| `tokio` | Async runtime, process spawning, mpsc/broadcast/oneshot channels |
| `tokio-util` | `CancellationToken` for graceful shutdown |
| `serde` / `serde_json` | JSONL protocol for ChildProcessWorker |
| `async-trait` | Async fn in trait (`Worker` trait) |
| `thiserror` | Ergonomic error types |
| `tracing` / `tracing-subscriber` | Structured logging |
| `uuid` | Unique `TaskId` generation |

---

## Future Extensions (not implemented)

- **HTTP API** (axum): wrap `ManagerHandle` with REST endpoints
- **Redis-backed SessionStore**: implement `SessionStore` trait with Redis client
- **Container Worker**: wrap `ChildProcessWorker` with Docker executor
- **CI CLI**: `ion submit`, `ion status`, `ion wait` commands
- **Wait queue**: when pool is full, queue acquire requests instead of failing

---

## Runtime Capability Matrix

| 能力 | Local | Remote | Sandbox | Router |
|------|:-----:|:------:|:-------:|:------:|
| `execute_command` | ✅ | ✅ SSH | ✅ sandbox-exec | ✅ |
| `read_file` | ✅ | ✅ SSH cat | — | ✅ |
| `write_file` | ✅ | ✅ SSH heredoc | — | ✅ |
| Permission Guard | ✅ | ✅ | ✅ | ✅ |
| UI Ask Channel | ✅ | ✅ | ✅ | ✅ |
| ProxyJump | — | ✅ | — | ✅ |
| macOS sandbox-exec | — | — | ✅ | ✅ |

## Known Limitations

- **EventBus** 当前使用 `Arc<Mutex<ExtensionEventBus>>`，大并发场景后续替换为 `broadcast + oneshot + AskRegistry`
- **CI 测试** 当前包含 manager 进程级测试，后续拆分为 in-process contract test + CLI smoke test
- **PermissionExtension** 与 `SecuredRuntime` 存在双层检查，后续统一为 `PolicyDecision` 聚合模型
- **WASM host functions** (`host_write_*_data` 等) 目前直接使用 `std::fs`，未经过 Runtime trait。后续需注入 Runtime 引用
- **SandboxRuntime** 只约束 `execute_command`，文件操作由 `PermissionEngine` 控制

## Version

```
ION Runtime Kernel v0.1
Local / Remote / Sandbox / Router runtime closed loop.
Security boundaries verified, CI reinforced, remote and sandbox paths tested.
```

Tags: `ion-runtime-p1`
