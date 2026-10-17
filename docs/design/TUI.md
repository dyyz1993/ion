# ION TUI 设计文档

> **状态：开发中** — Phase 0 规划定稿，Phase 1 脚手架未开始。

## 概览

ION TUI 是 ION 的第三个前端（CLI 直跑 / host+worker / **TUI**），复用同一个 host 进程和 IPC socket，只新增「视图 + 控制器」。它不做执行、不做调度、不写 ledger——所有状态修改走现有 RPC（`run.pause` / `run.resume` / `run.auto_resume`），所有状态读取走 `run.get` / `run.list` / `events.subscribe`。对标 lazygit（git）和 k9s（k8s）的交互模型。

| 能力 | 入口 | 状态 |
|------|------|------|
| durable run 列表（session/state/version） | `run.list` | 🔧 设计稿 |
| 选中 run 详情（tasks/reason/actions） | `run.get` | 🔧 设计稿 |
| 实时事件流（run_events 增量） | `events.subscribe` | 🔧 设计稿 |
| 键盘暂停 / 恢复 / auto_resume 切换 | `run.pause` / `run.resume` / `run.auto_resume` | 🔧 设计稿 |
| 断连重连 + 状态恢复 | 自动重连 + `run.list` 全量刷新 | 🔧 设计稿 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | workspace 成员 `ion-tui/` crate 脚手架 | 🔧 | `cargo check --workspace` |
| 1.2 | Unix socket 连接 + hello 握手 + 请求/响应信封 | 🔧 | 单元 mock socket |
| 1.3 | run.list 表格 + 选中态 | 🔧 | Faux host 集成 |
| 1.4 | run.get 详情面板 | 🔧 | Faux host 集成 |
| 1.5 | events.subscribe 增量流 + 游标推进 | 🔧 | Faux host 集成 |
| 1.6 | 键盘 p/r/a/q 操作 → RPC → 状态回读 | 🔧 | Faux host 集成 |
| 1.7 | 断连检测 + 自动重连 | 🔧 | 模拟 socket 关闭 |
| 1.8 | CLI 入口 `ion-tui` 二进制 | 🔧 | `ion-tui --help` |
| 1.9 | `tests/tui_smoke_ci.sh`（headless 模式） | 🔧 | bash 脚本 |
| 2.1 | 删除 `ion-web/` | 🔧 | CI 无引用残留 |

## 1. 架构与边界

```
┌──────────────┐    IPC socket (JSONL)   ┌──────────────┐
│   ion host   │◄───────────────────────►│   ion-tui    │
│  (进程权威)   │   run.list/get/subscribe │ (视图+控制器) │
└──────────────┘   run.pause/resume/...  └──────────────┘
        ▲                                    │
        │ spawn                              │ 渲染
        ▼                                    ▼
┌──────────────┐                     ┌──────────────┐
│   worker     │                     │  terminal    │
└──────────────┘                     └──────────────┘
```

**边界（硬性）**：

- TUI 不直接读/写 `execution.sqlite3`——一切状态以 RPC 返回为准。
- TUI 不做乐观更新——按键发出 RPC 后，等待 `run.get` 回读确认才更新本地状态。
- TUI 不内置 scheduler——不派发 worker、不做 auto-respawn，只调现有 RPC。
- TUI 不读 `sessions.index.json`——会话列表由 `list_sessions` RPC 提供。

## 2. 代码组织

```
ion/                        ← workspace root（monorepo）
├── ion-tui/                ← 新增 crate
│   ├── Cargo.toml          ← deps: ratatui, crossterm, tokio, ion-protocol, serde_json
│   └── src/
│       ├── main.rs         ← CLI 入口 + 事件循环启动
│       ├── connection.rs   ← Unix socket 连接/重连/请求-响应匹配
│       ├── state.rs        ← AppState: runs 列表、选中索引、事件缓冲
│       ├── ui.rs           ← ratatui 渲染（面板布局/表格/详情/事件流）
│       └── input.rs        ← 键盘事件 → Action 枚举 → RPC 调用
├── Cargo.toml              ← workspace.members += "ion-tui"
└── ion-web/                ← 删除
```

主仓 `ion` crate **不 import** `ion-tui`（避免编译依赖倒灌）；`ion-tui` import `ion-protocol`，依赖单向。

## 3. 状态模型

```rust
pub struct AppState {
    pub runs: Vec<RunListEntry>,          // run.list 快照
    pub selected: Option<usize>,          // 光标位置
    pub detail: Option<RunSnapshot>,      // 选中 run 的 run.get 快照
    pub events: VecDeque<EventLine>,      // 选中 run 的事件流（有界 1000 条）
    pub cursor: i64,                      // events.subscribe 游标
    pub connection: Connection,           // Connected / Reconnecting(u32)
}
```

### 数据流（单向）

```
键盘 → input.rs Action → connection.rs 发 RPC
   ↓（异步等响应）
connection.rs 收响应 → state.rs 更新 → ui.rs 渲染
```

禁止反向：UI 组件不直接改 state；state 不直接调 RPC（通过 Action 队列）。

## 4. 交互规格

### 全局键

| 键 | 动作 | RPC |
|----|------|-----|
| `j` / `↓` | 光标下移 | — |
| `k` / `↑` | 光标上移 | — |
| `Enter` | 展开选中 run 详情 + 订阅事件流 | `run.get` + `events.subscribe` |
| `p` | 暂停选中 run | `run.pause {runId, reason:"tui"}` |
| `r` | 恢复选中 run | `run.resume {runId}` |
| `a` | 切换 auto_resume | `run.auto_resume {runId, enabled:!current}` |
| `R` | 手动刷新列表 | `run.list` |
| `q` / `Esc` | 退出 | — |

### 操作反馈

所有写操作（pause/resume/auto_resume）发出后：
1. UI 底部状态栏显示 `→ run.pause {runId}` pending
2. RPC 响应到达 → `run.get` 回读 → 状态栏显示 `✓ state=paused`
3. 响应超时 5s → 状态栏显示 `✗ timeout`，状态不变

**禁止乐观更新**：只有回读确认后才改本地 `AppState`。

## 5. CLI 入口

```bash
# 二进制直接运行
ion-tui                          # 连接 ~/.ion/host.sock
ion-tui --socket /tmp/x.sock     # 自定义 socket
ion-tui --help

# 或者作为 ion 子命令（Phase 2 可选）
ion tui
```

环境变量：

- `ION_HOST_SOCKET`：socket 路径覆盖（与 `ion rpc` 同语义）
- `ION_TUI_HEADLESS=1`：无终端模式（CI 用），渲染一帧到 stdout 后退出

## 6. 测试策略

| 层 | 工具 | 覆盖 |
|----|------|------|
| 单元 | `cargo test -p ion-tui` | state.rs 纯函数、事件缓冲裁剪、游标推进 |
| 集成 | mock Unix socket host | 请求/响应信封匹配、超时、断连重连 |
| CLI | `tests/tui_smoke_ci.sh` | 起 host + `ION_TUI_HEADLESS=1 ion-tui` → 断言 stdout 包含 run 列表 → 按 `p`/`r` 后 `run.get` 状态正确 |
| 全量 | `cargo test --workspace` | ion-tui 加入后不破坏既有 |

### Faux host harness

`tests/tui_smoke_ci.sh` 复用 `durable_pause_resume_ci.sh` 的种子 DB + host 启动逻辑，追加 TUI headless 调用断言。

## 7. ion-web 删除计划

| 步骤 | 操作 | 验证 |
|------|------|------|
| 1 | `git rm -r ion-web/` | `rg 'ion-web' . --type-add 'all:*' -uu | grep -v .git` 无残留 |
| 2 | 删 `.github/workflows/*.yml` 中 ion-web 引用（如有） | `grep ion-web .github/` 空 |
| 3 | 删 `docs/` 中 ion-web 链接（如有） | `bash tests/docs_links_ci.sh` 绿 |
| 4 | gateway-mac.mjs（仓库外）不受影响 | 不在 git 内 |

## 8. 迭代切分

| Phase | 交付 | 验证门 |
|-------|------|--------|
| P1 | crate 脚手架 + socket 连接 + run.list 渲染 | `cargo check --workspace` + headless smoke |
| P2 | run.get 详情 + 键盘 p/r/a + 操作回读 | 集成测试 + smoke 脚本 |
| P3 | events.subscribe 增量流 + 断连重连 | 集成测试 |
| P4 | 删 ion-web + docs 清理 | `docs_links_ci` + CI 全绿 |
| P5 | tag `tui-v0.1` + 合并 master + 推送 | 全量 `cargo test --workspace` |

每个 Phase 独立 commit，出错收进 [docs/LESSONS.md](../LESSONS.md)。

## 9. 风险与备选

| 风险 | 缓解 |
|------|------|
| ratatui 事件循环阻塞 tokio runtime | crossterm EventStream 用 tokio::select!，不 block_on |
| socket 断连时 UI 假死 | 全部 RPC 带超时，断连后进入 Reconnecting 状态并轮询 |
| 终端 resize 渲染崩溃 | ratatui 内置处理，CatchUnwind 兜底恢复终端模式 |
| workspace 编译时间变长 | ion-tui 依赖少（无 wasm/reqwest），增量编译影响小 |

## 10. 备选方案（不采用）

- **继续堆 ion-web**：需要补 HTTP gateway + React 状态管理 + 样式，工作量大且偏离 CLI-first 定位。
- **TUI 做成独立项目仓库**：失去 workspace 测试联动，发版要同步两个仓。
- **用 egui/immediate mode**：跨平台渲染依赖重，终端场景 ratatui 更轻。

## 13. v0.9 附件路径选择与运行中工具卡（已实现）

- `Ctrl+A` 打开 Attach path 模态输入；Enter 确认加入附件（去重），Esc 取消；`x` 删除最后一个附件。
- 运行中任务派生设计 F 骨架卡：spinner + tool/kind + logical key + elapsed + 输出尾部（当前为 running… 与光标），后续接入真实流即替换尾部。
- Busy 语义保留 `Esc/s` 停止；审批卡仍优先锁定 composer。
- 验证：`cargo test -p ion-tui` 53/0、`tests/tui_smoke_ci.sh` 34/0、`docs_links_ci.sh` 373/0；截图 `~/Downloads/ion-tui-v0.9-attachment-live.png`。

## 14. v0.10 真实工具输出流（已实现）

- 新增 session 事件流订阅：`subscribe(session)` 保持长连接，解析 `tool_execution_start/update/end`。
- `tool_execution_start` 建立 live card；`partialResult` 逐行推入 10 行尾部；`end` 写入耗时与 completed 标记。
- 流事件按 sessionId 过滤，只作用于当前 Conversation；切换会话时重建流。
- 定时刷新不再覆盖真实流输出；同一 running task 的实时 tail 保持。
- 验证：`cargo test -p ion-tui` 57/0、`tests/tui_smoke_ci.sh` 34/0、`docs_links_ci.sh` 373/0；截图 `~/Downloads/ion-tui-v0.10-live-tool-stream.png`。

## 15. v0.11 审批实时刷新与完成工具沉淀（已实现）

- 会话工具流之外新增 UI 事件订阅；`ui_event.ui_type` 命中 `ApprovalRequest / ApprovalResolved / ApprovalRemoved / ApprovalReset / AskResolved / AskTimedOut` 时立即回读 `approvals_pending`，不做乐观删除。
- 双路原始流分别由独立 pump 聚合到一个事件通道，避免 UI 流空闲时阻塞 session 工具事件；审批变化与工具输出都能实时进入主循环。
- `tool_execution_end` 后的卡片写入 `completed_tools`（最多 10 张），作为历史折叠区展示；`e` 展开最近完成工具的输出尾部，运行中卡片继续保留在最新位置。
- 快照派生的 running 占位卡只在无真实流卡片时创建，避免周期刷新把已完成的真实工具输出替换回 `running…`。
- 截图转换使用 Menlo + Hiragino Sans GB 双字体：ASCII/线条用 Menlo，CJK 用 Hiragino，避免中文与符号在 PNG 中出现豆腐块；终端 capture 本身保持 UTF-8。
- 验证：`cargo test -p ion-tui` 59/0、`cargo clippy -p ion-tui --all-targets -- -D warnings` 0 warning、`tests/tui_smoke_ci.sh` 34/0、`docs_links_ci.sh` 373/0；截图 `~/Downloads/ion-tui-v0.11-realtime-approval-completed-tools.png`。

## 16. v0.12 完成工具卡片历史持久化（已实现）

- Worker 在 `on_tool_execution_end` 将完成卡片追加为 `custom/tool_card` JSONL entry；字段包含 `sessionId/toolCallId/toolName/args/isError/durationMs/resultTail/completed`。它是 UI 重建用的旁路轨迹，不进入 LLM 上下文，也不是新的 sidecar 文件。
- `StreamingExtension` 持有当前 worker 的 session 文件路径，避免依赖进程全局路径；本地与 fork/child 会话都写入对应 `<sid>.jsonl`。
- `FileIndex` live 索引与 `message_retrieval` 默认 custom 过滤显式保留 `tool_card`；TUI 将其渲染为历史轮次中的折叠工具卡，错误卡默认展开。
- `SessionHeader` 反序列化宽容保留扩展字段（如 `spawnMeta`），避免独立会话文件因 header 扩展字段被 typed reader 误判不存在；headless `--session` 读消息时对 host 关闭 idle write half 的连接执行重连重读。
- 真实审批 E2E 使用 host-shaped 双 socket：UI 事件流推送 `ApprovalRequest`/`ApprovalResolved`，TUI 事件驱动回读 `approvals_pending`；approve 后不乐观删除，以空列表回读为准。
- 验证：`cargo test -p ion-tui`、`cargo test -p ion message_retrieval`、`cargo test -p ion file_index`、`tests/tui_smoke_ci.sh`（含 35 项断言：完成工具卡历史、审批实时出现/消失、SIGKILL 恢复回归）、`cargo fmt --all --check`、`cargo clippy -p ion-tui --all-targets -- -D warnings`、`docs_links_ci.sh`。

## 17. v0.13 Simple Chat Mode（已实现）

- `ion-tui --chat` 默认进入 chat-first 界面：顶部仅保留 ION / model / context / 快捷键提示，中栏为自然聊天流，底部为单一 composer；不显示 Sessions、Runs、Events、Tasks 调试面板。
- Simple Chat 复用现有会话、消息、审批与工具流 RPC，不新增后端接口；空会话显示 `开始一段新对话`。
- 历史工具卡片默认折叠为 `▸ [tool] ...`，当前 live tool 保留实时状态；审批卡置顶显示，`y/n` 复用现有 approve/reject 回读路径，composer 在审批期间锁定为等待决策。
- `d` 在 Simple Chat 与三栏 Debug Workbench 之间切换；切回调试模式时可继续使用 `1/2/3/4`、`j/k`、`p/r/a/e` 等既有键位。Busy 状态下 Simple Chat 的 composer 标题显示 `Esc stop`。
- 验证：`cargo test -p ion-tui` 62/0（含 Simple Chat 渲染/模式切换）、`tests/tui_smoke_ci.sh` 39/0（T8 断言隐藏 Sessions/Tasks、显示空状态）、`cargo fmt --all --check`、`cargo clippy -p ion-tui --all-targets -- -D warnings`、`cargo test --workspace --lib --bins --tests`、`docs_links_ci.sh`。
