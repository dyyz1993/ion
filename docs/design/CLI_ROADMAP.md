# CLI 落地路线图

> **状态：已完成** — Phase 0-5 全部落地。`ion serve` / `ion --host` / 场景 2 编排 / 递归 idle 全部可用。

---

## Phase 0：准备（~0.5天）

> 先修已知 bug 和警告，避免干扰后续开发。

| # | 任务 | 文件 | 工作量 |
|---|------|------|--------|
| 0.1 | 修 5 个 unreachable pattern（ion_worker.rs 匹配重复分支，是逻辑 bug） | `src/bin/ion_worker.rs` | ~30min |
| 0.2 | `cargo fix --lib -p ion` + `cargo fix -p ion-provider`（自动修 40+ 个警告） | 全局 | ~10min |
| 0.3 | 手动扫剩余 ~50 个警告（删无用 import、加 `_` 前缀、去多余 `mut`、snake_case） | 多个文件 | ~1h |

**验收：** `cargo build` 0 warnings

---

## Phase 1：rename `manager` → `host`（~0.5天）

> 纯改名，不动逻辑。用户可见的 "manager" 全部换成 "host"。

| # | 任务 | 改前 | 改后 | 工作量 |
|---|------|------|------|--------|
| 1.1 | 新增 `ion serve` 别名（内部调 cmd_manager_start） | `ion manager start` | `ion serve` | ~30min |
| 1.2 | 新增 `ion serve stop`（发 shutdown RPC） | - | `ion serve stop` | ~30min |
| 1.3 | 新增 `ion serve status`（检查 PID 文件 + 进程存活） | - | `ion serve status` | ~30min |
| 1.4 | 隐藏/废弃 `ion manager start`（保留兼容但不展示在 help） | `ion manager start` | ✅ 仍可用，help 不显示 | ~10min |
| 1.5 | socket 路径改名 | `manager.sock` | `host.sock` | ~10min |
| 1.6 | PID 路径改名 | `manager.pid` | `host.pid` | ~10min |
| 1.7 | 更新 AGENTS.md 架构部分 | `manager` 相关 | `host` + `serve` | ~15min |

**注意：** 代码内部的 `Manager` struct、`manager.rs`、`WorkerRegistry` 等**不改**——"manager" 只在 CLI 层面消失。

---

## Phase 2：`--host` 标志位（~1天）

> 让 `ion --host "做这个"` 能工作：自动启动 WorkerRegistry，所有 Worker idle 自动退出。

| # | 任务 | 说明 | 工作量 |
|---|------|------|--------|
| 2.1 | 给 Cli 结构体加 `--host` flag | `--host` 是 bool flag | ~15min |
| 2.2 | 实现 `--host` 逻辑：启动临时 WorkerRegistry + 事件泵 + spawn coordinator + 等 idle 自动关 | 对标现有 cmd_team()，去掉硬编码成分 | ~3h |
| 2.3 | 删掉 `cmd_team()` 和 `ion team` 子命令 | 已被 `--host --agent coordinator` 覆盖 | ~15min |
| 2.4 | 验证 spawn_worker 在场景 1 中报错提示"用 --host" | 在工具执行层加检测 | ~30min |

**验收：**
- `ion --host "spawn_worker 分给两个子 Worker 做"` → 自动启停
- `ion "spawn_worker xxx"` → 提示用 `--host`

---

## Phase 3：场景 1 spawn_worker 不可用报错（~0.5天）

> 确保场景 1 下调用 spawn_worker/channel_send 等需要 host 的工具时，给用户明确的错误提示。

| # | 任务 | 工作量 |
|---|------|--------|
| 3.1 | 在 spawn_worker 工具执行时检测是否有 host | ~30min |
| 3.2 | 在 channel_send 工具执行时检测是否有 host | ~15min |
| 3.3 | 在 send_to_worker 工具执行时检测是否有 host | ~15min |

---

## Phase 4：退出条件实现（~0.5天）

> 实现递归 idle 检测，确保 WorkerTree 全部完成才退出。

| # | 任务 | 工作量 |
|---|------|--------|
| 4.1 | WorkerRegistry 加 `all_workers_idle()` 方法（DFS 递归检测） | ~30min |
| 4.2 | cmd_team 改造成使用递归 idle 检测，而不是当前简单 busy map | ~30min |
| 4.3 | 超时兜底：30 分钟无变化强制退出 | ~15min |

---

## Phase 5：文档同步（~0.5天）

| # | 任务 | 工作量 |
|---|------|--------|
| 5.1 | 更新 CLI_ARCHITECTURE.md 验证用例为真实 CLI | ~30min |
| 5.2 | 更新 AGENTS.md 路线图状态 | ~15min |
| 5.3 | ✅ 已完成：TEAM_ARCH.md 归档，新方案 TEAM_ORCHESTRATION.md | done |

---

## 总计

| Phase | 工作量 | 产出 |
|-------|--------|------|
| P0 修 warnings + bug | ~2h | 0 warnings 构建 |
| P1 manager → serve | ~2h | `ion serve` / `ion serve stop` / `ion serve status` |
| P2 --host 标志位 | ~4h | `ion --host "做这个"` 完整链路 |
| P3 报错提示 | ~1h | 场景 1 调 host 工具时友好提示 |
| P4 退出条件 | ~1h | 递归 idle 检测，正确自动关 |
| P5 文档 | ~1h | 文档同步 |
| **合计** | **~11h** | 三场景完整落地 |

---

## 依赖关系

```
P0（修 bug）              无依赖，最先做
  │
P1（rename + serve）      无依赖，可并行
  │
P2（--host 实现）         依赖 P1（用了新命名）
  │
P3（报错提示）            与 P2 并行
  │
P4（递归退出）            依赖 P2（--host 的 host 需要它）
  │
P5（文档）                全部完成后
```
