# Session Workspace Chat Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在当前 Session 中创建一个带独立 Git worktree 的新 Session，并通过结果卡片或会话列表点击进入该 Session。

**Architecture:** 内核负责 Worktree、Session、Worker 生命周期和可恢复事件；UI 负责结果卡片、active-session 路由和 Session 页面切换。页面跳转不重绑已有 RPC client，而是按 `sessionId + projectPath + sessionPath` 拉取快照并重新订阅。

**Tech Stack:** Rust、ION Unix-socket JSONL RPC、EventBus、Git worktree、现有 `ion-orbit-ui` HTML/CSS/JavaScript、FauxProvider Harness、Shell CLI 测试。

详细需求、接口、目录策略和自检门禁见：[SESSION_WORKSPACE_CHAT.md](../design/SESSION_WORKSPACE_CHAT.md)。

---

### Task 1: 完成开发前自检

**Files:**

- Read: `docs/design/SESSION_WORKSPACE_CHAT.md`
- Read: `src/bin/ion.rs`
- Read: `src/worker_registry.rs`
- Read: `docs/design/SESSION_TREE.md`

**Step 1: 确认语义**

- 确认目标是“当前 Session 创建子 Session 并点击跳转”；
- 确认 Session B 的 `parentSessionId`、Worktree、清理策略；
- 确认原型不连接真实 Host。

**Step 2: 确认现有能力**

```bash
rg -n "create_session|create_worker|worktree|subscribe|fork_from_leaf" src docs/design
```

Expected: 找到现有 Session Tree、Worker Worktree、RPC 和订阅实现，并记录需要复用的位置。

**Step 3: 通过门禁**

只有需求文档第 6 节的关键项全部有答案后，才进入代码开发。

### Task 2: 实现 WorkspaceSession 数据模型

**Files:**

- Modify: `src/session_index.rs`
- Modify: `src/worker_registry.rs`
- Create if needed: `src/session_workspace.rs`
- Test: `tests/session_workspace_harness.rs`

**Step 1: Write the failing Harness test**

验证创建结果包含 `sessionId`、`parentSessionId`、`workspacePath`、`branch` 和 `status`。

**Step 2: Run the test**

```bash
cargo test --test session_workspace_harness
```

Expected: FAIL，因为统一 WorkspaceSession 数据还不存在。

**Step 3: Implement the minimal model**

增加可序列化的 workspace 元数据，并让 SessionIndex 能持久化和恢复。

**Step 4: Run the test again**

```bash
cargo test --test session_workspace_harness
```

Expected: PASS。

### Task 3: 实现统一创建 RPC

**Files:**

- Modify: `src/bin/ion.rs`
- Reuse: `src/worker_registry.rs`
- Test: `tests/session_workspace_ci.sh`

**Step 1: Write the failing CLI case**

调用 `create_workspace_session`，断言返回 Session B 和 Worktree 元数据。

**Step 2: Run the case**

```bash
bash tests/session_workspace_ci.sh
```

Expected: FAIL，因为 RPC 尚未注册。

**Step 3: Implement the atomic creation flow**

按顺序创建 Worktree、Session、Worker，失败时删除半成品并广播失败事件。

**Step 4: Run the case again**

```bash
bash tests/session_workspace_ci.sh
```

Expected: PASS。

### Task 4: 实现恢复快照和实时事件

**Files:**

- Modify: `src/bin/ion.rs`
- Modify: `src/event_bus.rs`
- Modify: `src/worker_registry.rs`
- Test: `tests/session_workspace_harness.rs`

**Step 1: Write recovery assertions**

验证先调用 `get_session_snapshot` 可以恢复 Session B，再通过 `subscribe --replay` 收到后续事件。

**Step 2: Run the test**

```bash
cargo test --test session_workspace_harness
```

Expected: FAIL，直到快照接口和事件类型完成。

**Step 3: Implement Pull then Push**

新增 `get_session_snapshot`，并统一 `workspace_session_creating/created/ready/failed/closed` 事件。

**Step 4: Run the test again**

```bash
cargo test --test session_workspace_harness
```

Expected: PASS。

### Task 5: 实现 HTML 原型

**Files:**

- Create: `ion-orbit-ui/pages/session-workspace-demo.html`

**Step 1: Implement Mock UI**

使用 Mock Session 数据模拟结果卡片、创建状态、Session 列表、hash 路由和返回操作。

**Step 2: Run the static server**

```bash
python3 -m http.server 8787 --directory ion-orbit-ui
```

Open:

```text
http://127.0.0.1:8787/pages/session-workspace-demo.html
```

Expected: 点击“打开工作树聊天”后地址变为 `#/sessions/<new-session-id>`，页面显示 Session B。

### Task 6: 完成命令行和真实场景验证

**Files:**

- Create: `tests/session_workspace_ci.sh`
- Optional Create: `tests/session_workspace_e2e.rs`

**Step 1: Run Harness**

```bash
cargo test --test session_workspace_harness
```

**Step 2: Run CLI verification**

```bash
bash tests/session_workspace_ci.sh
```

**Step 3: Run real LLM case when credentials are available**

```bash
ION_E2E=1 cargo test --test session_workspace_e2e -- --ignored
```

**Step 4: Run final checks**

```bash
git diff --check
```

Expected: all tests pass and no whitespace errors are reported.
