# Durable Execution ADR and Iteration Plan

> **状态：开发中** — PR0 契约已落地；ExecutionStore、恢复调度与故障矩阵按阶段实现。

## ADR: SQLite execution ledger is a transaction authority

ION's session-derived state rule forbids sidecar files for data that belongs to one session. Durable execution is different: `~/.ion/execution.sqlite3` stores a cross-run, cross-task transaction graph while the process is running. It is not a session cache.

- `execution.sqlite3` is authoritative for durable runs, tasks, attempts, resume operations, transactional events, and outbox effects.
- `sessions.index.json` remains a small directory index. It may expose hot run identifiers, but never participates in execution-state decisions.
- Session JSONL remains conversation history and export projection. A projector may append durable transcript entries, but JSONL is never a second execution authority.
- A durable run must never fall back to the legacy auto-respawn prompt path. Failure disables dispatch until reconciliation produces a recovery plan.

The first production slice is intentionally narrow: local, single-root, single-branch runs with no child runs or remote execution. Those cases stay blocked until later phases.

## Identity and state machine

- `runId`: one persistent execution attempt graph. A session can reference multiple runs.
- `taskId`: one logical model/tool/wait unit.
- `attemptId`/`attempt_no`: one physical execution attempt and fencing generation.
- `workerId`: volatile process identity; it never identifies durable work by itself.

Run states are `queued`, `running`, `recovering`, `waiting_children`, `waiting_approval`, `paused`, `blocked`, `succeeded`, `failed`, and `cancelled`. Terminal states never become non-terminal. A late event can update a task attempt record but cannot revive its run.

The first slice only requires `queued`, `running`, `recovering`, `blocked`, `paused`, and terminal states. Child-wait and approval-wait states remain reserved.

## Recovery policy

Every tool has a recovery policy, defaulting to `unsafe`.

- `replay_safe`: no external effect. Existing committed results are reused; pending attempts may execute for the first time after restart.
- `unsafe`: shell, writes, external messages, purchases, and other unverified side effects. If an attempt reached `executing` without a committed result, recovery marks it `uncertain` and blocks the run.
- `idempotent_external`: reserved for tools whose target verifies a stable effect key. Until such tools exist, they are treated as unsafe.

Recovery never means approval of unknown effects, arbitrary process revival, token-level model continuation, or exactly-once external operations.

## Public interface

- `run.get {runId}` returns one coherent snapshot: run state, state version, task summary, cursor, blockers, and allowed actions. It never dispatches work.
- `run.resume {runId, requestId, expectedStateVersion, recoveryPlanId?}` durably accepts a resume intent and is idempotent by `(principal, runId, requestId)`. It accepts no free prompt, tool arguments, or force flag.
- `events.subscribe {runId, afterCursor}` emits contiguous ledger events at least once, followed by `caught_up`; a missing notification causes a database re-read, never a silent gap.

## Iterations

1. **PR0 - Contract cleanup:** approve this boundary, reserve stable API/state names, and rename the existing Web action to “new relay task” so it no longer claims durable recovery.
2. **PR1 - Minimal store:** opt-in config, schema, transactional writer, snapshots, CAS resume, and event/outbox tests without dispatch.
3. **PR2 - Local execution closure:** checkpoint model requests/tool batches, tool start/end, tool recovery policies, and JSONL projection.
4. **PR3 - Host reconciliation and APIs:** host epoch fencing, recovery plans, public RPC commands, and CLI verification.
5. **PR4 - UI state model:** separate connection and execution state, cursor-based resync, and idempotent resume UX.
6. **PR5 - Graph and approvals:** persistent child edges/inputs and approval invalidation.
7. **PR6 - Failure hardening:** kill/failure matrix and release gate.

## CLI verification

Automated verification is `tests/durable_execution_ci.sh`. PR3 exposes host-level `run.get`, `run.resume`, and paginated `events.subscribe`; host startup initializes the ledger and performs epoch reconciliation. Current subscription RPC returns a durable event page and is a pull substrate for the PR4 live subscriber; clients must not infer execution state from socket liveness. PR1/PR2 store tests remain the transaction/recovery source of truth. Real-LLM cases are ignored unless `ION_E2E=1`.
