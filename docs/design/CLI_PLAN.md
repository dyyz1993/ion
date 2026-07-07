# CLI 完整落地方案

> **状态：待执行** — 合并 CLI_ARCHITECTURE.md（架构设计）与 CLI_ROADMAP.md（落地路线），加入每个任务的执行清单。这是推进的唯一入口文档。
>
> **目标：** 三种 CLI 执行场景全部落地，所有验证用例通过。

---

## 目录

1. [架构设计](#一架构设计)
2. [最终 CLI 命令树](#二最终-cli-命令树)
3. [落地路线图（带 checklist）](#三落地路线图带-checklist)
4. [验证用例（CLI_ARCHITECTURE.md）](#四验证用例)
5. [跨场景对比矩阵](#五跨场景对比矩阵)

---

## 一、架构设计

### 1.1 三场景归属：两套引擎

场景 1 是**直接执行**（没有 host）。场景 2 和场景 3 共享同一套**host 引擎**（WorkerRegistry + 事件转发 + spawn_worker），区别只在对外暴露方式不同：

```
              ┌─ 场景 1：直接 spawn 子进程，不经过 host
              │   跑完即退，没有事件转发
              │
    同一套     ├─ 场景 2：临时 host + 事件泵 → stdout
    底层 API  │   递归 idle 自动关
    (spawn、   │
     await、  └─ 场景 3：常驻 host + Unix socket → 外部 UI
    channel)      不自动退，外部可全程接入
```

| 场景 | CLI | 引擎 | 事件出口 | 同步子任务 | 异步任务 | 退出方式 |
|------|-----|------|---------|-----------|---------|---------|
| **1. 快速执行** | `ion "做这个"` | 直接 spawn（无 host） | ❌ 无 | ✅ spawn→await | ❌ 进程退出子 Worker 被干掉 | 跑完即退 |
| **2. 快速编排** | `ion --host "做这个"` | host 引擎 | 事件泵 → stdout | ✅ | ✅ host 兜着 | 递归 idle 自动关 |
| **3. 常驻服务** | `ion serve` | host 引擎 + socket | socket → 外部 UI | ✅ | ✅ host 兜着 | 手动 shutdown |

### 1.2 三场景流程图

**场景 1：**

```
终端                   进程内
┌──────┐   ┌──────────────────────────┐
│      │   │  cmd_run()               │
│ ion  │──→│  建工具集 + Agent        │
│      │   │  agent.run(message)      │
│      │   │    ├─ LLM 循环            │
│      │   │    ├─ 调 tool (read/write)│
│      │   │    ├─ spawn_worker(同步)  │
│      │   │    │    └─ spawn 子进程    │
│      │   │    │        await 等完    │
│      │   │    └─ 返回               │
│      │   └─ 进程退出                  │
└──────┘                              │
    ❌ 没有 host，不能异步              │
    ❌ 没有事件转发                     │
    ✅ 同步子任务能用                    │
```

**场景 2：**

```
终端                              临时 host
┌──────┐  ┌──────────────────────────────────────────────┐
│      │  │  WorkerRegistry + 命令循环 + 事件泵           │
│ ion  │──│                                              │
│      │  │  spawn coordinator Worker (子进程)            │
│--host│  │    │                                          │
│      │  │    ├─ spawn_worker(dev, 同步)                 │
│      │  │    │    └─ host 创建子 Worker → await 完成   │
│      │  │    ├─ spawn_worker(dev, 异步)                 │
│      │  │    │    └─ host 创建子 Worker                 │
│      │  │    │       └─ 子 Worker 执行 → agent_end      │
│      │  │    └─ channel_send ← 子 Worker 过程通信      │
│      │  │                                              │
│      │  │  事件泵 → stdout (实时打印 text_delta)        │
│      │  │  ...全部 idle → 清理退出                      │
└──────┘  └──────────────────────────────────────────────┘

    ✅ 有 host，同步异步都行
    ✅ 事件泵 → stdout
    ❌ 没有 socket，外部工具接不了
```

**场景 3：**

```
外部 UI / TUI / IDE 插件               常驻 host
┌─────────────────┐   ┌───────────────────────────────────────┐
│        socket    │   │  WorkerRegistry + 命令循环            │
│  Web UI          │   │  Unix socket → ~/.ion/host.sock      │
│  ┌───────────┐   │   │                                       │
│  │进度条     │   │   │  spawn Worker(子进程)                  │
│  │卡片       │◄──│───│  ├─ 同步：spawn → await （UI 可见）   │
│  │步骤状态   │   │   │  │  └─ 通过 socket 推 text_delta      │
│  │实时日志   │   │   │  ├─ 异步：spawn → agent_end（UI 可见）│
│  └───────────┘   │   │  │  └─ 通过 socket 推 agent_start    │
│                  │   │  │        → text_delta → agent_end    │
│  ion rpc 命令行  │   │  ├─ channel_send ← 过程通信          │
│  ┌───────────┐   │   │  ├─ subscribe → 事件流推给 socket    │
│  │create_   │───│───│  └─ 一直运行（不自动退）               │
│  │worker     │   │   │                                       │
│  └───────────┘   │   │                                       │
└─────────────────┘   └───────────────────────────────────────┘

    ✅ 有 host，同步异步都行
    ✅ 事件通过 socket 推给外部工具 ── UI 可渲染成卡片/进度条
    ❌ 不自动退出，需要手动 shutdown
```

### 1.3 同步子任务 vs 异步任务

```
同步子任务 (spawn + await)         异步任务 (spawn + agent_end)
───────────────────────────       ───────────────────────────
Agent: spawn_worker(dev,         Agent: spawn_worker(dev,
       "查文档")                       "监控日志")
Agent: await_worker(id)          Agent: 继续聊别的
       ────干活────                       ──子 Worker 发消息──
Agent: ← 拿结果                          channel_send 实时收
                                       ──子 Worker agent_end──
                                        host 检测到 → UI 更新
```

> `channel_send` 是**工作过程中**的通信（子 Worker 还在跑时跟 coordinator 交流进度、问问题），不是完成通知。完成通知通过 `agent_end` 事件检测。

### 1.4 退出条件（场景 2）

递归 idle 检测：

```
入口 Worker (coordinator) idle？
├─ 它 spawn 的子 Worker 1 idle？
│   └─ 子 Worker 的子 Worker idle？
├─ 子 Worker 2 idle？
└─ ...全部 idle
  → 没有后台进程在跑 → 清理退出
```

> 如果需要反复执行（loop），外面套一个 shell while 即可，底层该退出退出，该启动启动。

---

## 二、最终 CLI 命令树

```
ion <message>...                            场景 1：快速执行
                                            单进程，跑完即退

ion --host <message>...                     场景 2：快速编排
                                            临时 host，全部 idle 自动关

ion serve                                   场景 3：启动常驻服务
ion serve stop                              场景 3：停止常驻服务（发 shutdown RPC）
ion serve status                            场景 3：检查服务状态（PID + 进程存活）

ion rpc --method ...                        场景 3：调试用的 RPC 接口（保留）
ion subscribe --session <id>                场景 3：调试用的事件订阅（保留）

ion-worker --mode rpc                       内部 Worker 子进程（不暴露给用户）
```

### 改名映射

| 改前 | 改后 | 说明 |
|------|------|------|
| `ion manager start` | `ion serve` | 改名 |
| - | `ion serve stop` | 新增 |
| - | `ion serve status` | 新增 |
| `~/.ion/manager.sock` | `~/.ion/host.sock` | socket 路径 |
| `~/.ion/manager.pid` | `~/.ion/host.pid` | PID 文件 |
| `ion team` | 删除 | 被 `ion --host --agent coordinator` 覆盖 |
| `ion manager start` | help 不显示（保留兼容） | 兼容旧脚本 |

> **代码内部的 `Manager` struct、`manager.rs`、`WorkerRegistry` 等不改**——"manager" 只在 CLI 层面消失。

---

## 三、落地路线图（带 checklist）

### Phase 0：修 bug + warnings（~2h）

> 先清理后顾之忧，避免干扰后续开发。

#### 任务清单

- [ ] **0.1** 修 5 个 unreachable pattern（`src/bin/ion_worker.rs` 中重复的 match arms）
  - 涉及命令：`get_context_usage` / `get_active_tools` / `call_tool` / `set_follow_up_mode` / `get_full_messages`
  - 每个 arm 在早期简化和后期完整实现各出现一次，第二个永不执行
  - **判断哪个是对的，删错的**
- [ ] **0.2** `cargo fix --lib -p ion` + `cargo fix -p ion-provider`（自动修 40+ 警告）
- [ ] **0.3** 手动扫剩余 ~50 个警告
  - 删无用 import（HashMap、Mutex、ToolRegistry、BufReader 等）
  - 加 `_` 前缀（rt/req/sessions/wid/sid/session 等）
  - 去多余 `mut`（7 个）
  - session_jsonl.rs 的 `parentId`/`modelId`/`thinkingLevel` 等加 `#[serde(rename = "...")]`
  - dead code 函数：`read_worker_stdout()`（worker_registry.rs）、`cmd_submit_old()`（ion.rs）→ 直接删除

#### 验证清单

- [ ] `cargo build` 0 warnings
- [ ] `cargo test --lib` 146 passed / 0 failed
- [ ] `cargo test`（全套）0 failed

---

### Phase 1：rename `manager` → `serve`（~2h）

> 纯改名 + 新增 2 个子命令，不动逻辑。

#### 任务清单

- [ ] **1.1** 新增 `ion serve` 子命令（内部调用 `cmd_manager_start`）
  - 位置：`src/bin/ion.rs` 的 `Commands` enum + main 分发
- [ ] **1.2** 新增 `ion serve stop`（向 host socket 发 shutdown RPC）
- [ ] **1.3** 新增 `ion serve status`（读 PID 文件 + `kill(pid, 0)` 检测存活）
- [ ] **1.4** 隐藏 `ion manager start`（保留兼容但 help 不展示）
- [ ] **1.5** socket 路径改名 `manager_socket_path()` → 返回 `~/.ion/host.sock`
  - 位置：`src/paths.rs`
  - 加自动迁移逻辑：如果新路径不存在但旧路径存在，软链或复制
- [ ] **1.6** PID 路径改名 `manager_pid_path()` → 返回 `~/.ion/host.pid`
- [ ] **1.7** 更新 AGENTS.md 架构部分所有 `manager start` → `serve`

#### 验证清单

- [ ] `ion serve` 启动，`~/.ion/host.sock` + `~/.ion/host.pid` 存在
- [ ] `ion serve status` 在运行时显示 `✔ Host running (pid xxx)`，未运行时显示 `✘ Host not running`
- [ ] `ion serve stop` 能正常停机，socket/pid 文件清理
- [ ] `ion serve` + `ion serve`（重复）→ 报错 `Host already running`
- [ ] `kill -9 <pid>` + `rm ~/.ion/host.pid` + `ion serve` → 自动清理 stale socket 启动成功
- [ ] 旧脚本 `ion manager start` 仍可用（兼容）

---

### Phase 2：`--host` 标志位（~4h）

> 让 `ion --host "做这个"` 工作：自动启动 WorkerRegistry，所有 Worker idle 自动退出。

#### 任务清单

- [ ] **2.1** 给 `Cli` 结构体加 `--host` flag（`#[arg(long)] host: bool`）
- [ ] **2.2** 实现 `cmd_host()` 函数
  - 启动临时 `WorkerRegistry`
  - 启动事件泵（订阅所有 Worker，转发 text_delta 到 stdout）
  - 启动 Manager command 处理循环（spawn_worker / channel_send 回传）
  - spawn 入口 Worker（agent 默认 build，可用 `--agent` 覆盖）
  - 等全部 idle（递归检测）→ 清理退出
  - 超时兜底：30 分钟
- [ ] **2.3** 删掉 `cmd_team()` 和 `ion team` 子命令
- [ ] **2.4** 在 main 分发中：`cli.host == true` 时走 `cmd_host()`，否则走 `cmd_run()`

#### 验证清单

- [ ] `ion --host "说 hello"` → 自动启停
- [ ] `ion --host "spawn_worker 分给两个子 Worker 做"` → 完整链路
- [ ] `ion --host --agent coordinator "分析项目"` → 用 coordinator agent
- [ ] 全部 idle 后 host 自动退出
- [ ] 30 分钟超时兜底强制退出（可调短测试）
- [ ] 没有 zombie 进程

---

### Phase 3：场景 1 调 host 工具时报错（~1h）

> 确保场景 1 下调用需要 host 的工具时给用户明确提示。

#### 任务清单

- [ ] **3.1** `spawn_worker` 工具执行前检测：有没有 host（ManagerBridge 存在？）
- [ ] **3.2** `channel_send` 工具执行前检测
- [ ] **3.3** `send_to_worker` 工具执行前检测
- [ ] **3.4** `await_worker` / `resume_worker` / `kill_worker` 工具执行前检测

> 统一报错信息：`spawn_worker requires a running host (use --host or ion serve)`

#### 验证清单

- [ ] `ion "调 spawn_worker 创建子 Worker"` → 报错提示用 `--host`
- [ ] `ion "调 channel_send 发消息"` → 报错提示用 `--host`
- [ ] 报错后不挂起、不死锁、不崩溃
- [ ] 报错信息明确告诉用户解决方案

---

### Phase 4：递归 idle 检测（~1h）

> 确保场景 2 退出条件正确：WorkerTree 全部完成才退出。

#### 任务清单

- [ ] **4.1** `WorkerRegistry` 加 `all_workers_idle(entry_id)` 方法
  - DFS 递归：入口 Worker → 子 Worker → 子子 Worker
  - 任一非 idle 立即返回 false
- [ ] **4.2** `cmd_host()` 改用 `all_workers_idle()` 检测退出
  - 替换现有的简单 busy map
- [ ] **4.3** 超时兜底：30 分钟无变化强制退出（`host_timeout` 环境变量可覆盖）

#### 验证清单

- [ ] `ion --host "spawn_worker(A → spawn_worker(B → spawn_worker(C)))"` → 按 C → B → A 顺序退出
- [ ] 混合同步+异步：异步未完成时不退出
- [ ] 超时强制退出，清理所有 Worker

---

### Phase 5：文档同步（~1h）

#### 任务清单

- [ ] **5.1** 更新 `docs/design/CLI_ARCHITECTURE.md` 验证用例 → 用真实 CLI（`ion serve` 而非 `ion manager start`）
- [ ] **5.2** 更新 `AGENTS.md` 路线图状态：Phase 0-5 全部标记 ✅
- [x] **5.3** ~~更新 `docs/design/TEAM_ARCH.md`~~ → 已归档到 `docs/archive/TEAM_ARCH.md`，新方案见 `docs/design/TEAM_ORCHESTRATION.md`（agent.md 驱动）
- [ ] **5.4** 归档 `CLI_ROADMAP.md` 内容到本文档（本文档成为唯一入口）

---

### 总计

| Phase | 工作量 | 产出 |
|-------|--------|------|
| P0 修 warnings + bug | ~2h | 0 warnings 构建 |
| P1 manager → serve | ~2h | `ion serve` / `stop` / `status` |
| P2 --host 标志位 | ~4h | `ion --host "做这个"` 完整链路 |
| P3 报错提示 | ~1h | 场景 1 调 host 工具时友好提示 |
| P4 递归退出 | ~1h | 正确自动关 |
| P5 文档 | ~1h | 文档同步 |
| **合计** | **~11h** | 三场景完整落地 |

### 依赖关系

```
P0（修 bug）              无依赖，最先做
  │
P1（rename + serve）      无依赖，可与 P0 后段并行
  │
P2（--host 实现）         依赖 P1（用了新命名）
  │
P3（报错提示）            与 P2 并行
  │
P4（递归退出）            依赖 P2（--host 的 host 需要它）
  │
P5（文档）                全部完成后
```

---

## 四、验证用例

> 以下是每个场景的分组验证用例，每个 Phase 完成后跑对应的 Group。

### 场景 1：快速执行 `ion "做这个"`

#### Group A1-1：基础执行

**A1-1-1 无参数**

```bash
ion
```

预期：

```
error: requires a message argument
Usage: ion [OPTIONS] <message>...
```

验证点：
- [ ] 友好的错误提示

**A1-1-2 基本对话**

```bash
ion "说 hello"
```

预期：返回 LLM 回复，进程退出。

验证点：
- [ ] Agent 直接执行
- [ ] 没有后台进程残留
- [ ] 进程退出

**A1-1-3 工具调用**

```bash
ion "帮我读 Cargo.toml"
```

验证点：
- [ ] Agent 调 read 工具
- [ ] 返回结果
- [ ] 进程退出

#### Group A1-2：同步子任务

**A1-2-1 spawn → await**

```bash
ion "调 spawn_worker 创建一个子 Worker 查 Rust 异步编程，await 等他回来，然后汇总"
```

验证点：
- [ ] spawn_worker 创建子 Worker
- [ ] await_worker 等待返回
- [ ] 子 Worker 完成后继续主流程
- [ ] 进程退出

**A1-2-2 多层 spawn**

```bash
ion "调 spawn_worker 创建一个子 Worker A，A 再创建一个子 Worker B"
```

验证点：
- [ ] 多层嵌套 spawn
- [ ] 每层 await 等回来
- [ ] 进程退出

**A1-2-3 spawn 后不 await**

```bash
ion "调 spawn_worker 创建一个子 Worker 但不 await 等它"
```

验证点：
- [ ] 主任务完成进程退出
- [ ] 子 Worker 被强杀（无 host）
- [ ] 不挂起

#### Group A1-3：边界与错误

**A1-3-1 spawn_worker 无 host 时报错**

```bash
ion "调 spawn_worker 创建一个子 Worker"
```

预期：

```json
{"error": "spawn_worker requires a running host (use --host or ion serve)"}
```

验证点：
- [ ] 工具执行时报错
- [ ] 提示用 `--host` 或 `ion serve`
- [ ] 不挂起

**A1-3-2 channel_send 无 host 时报错**

```bash
ion "调 channel_send 发一条消息"
```

验证点：
- [ ] 无 host 时报错
- [ ] 不挂起

**A1-3-3 子 Worker 超时**

```bash
ion "调 spawn_worker 创建一个子 Worker 跑 sleep 60，await 等 5 秒超时"
```

验证点：
- [ ] await_worker 带超时
- [ ] 超时后子 Worker 被终止
- [ ] 主流程继续

**A1-3-4 异步不可用提示**

```bash
ion "spawn_worker 创建一个监控日志的异步任务，然后继续聊天"
```

验证点：
- [ ] Agent 知道场景 1 不支持异步
- [ ] 引导用户使用 `--host`

---

### 场景 2：快速编排 `ion --host "做这个"`

#### Group A2-1：host 启停

**A2-1-1 host 自动启动**

```bash
ion --host "说 hello"
```

预期：

```
[host] 启动 WorkerRegistry
[pump] spawning coordinator...
[pump] [wkr_xxx] Hello! ...
[pump] [wkr_xxx] agent_end
[host] 递归 idle 检测通过
[host] 清理退出
```

验证点：
- [ ] host 自动拉起
- [ ] 事件泵实时输出
- [ ] 全部 idle 自动退出

**A2-1-2 快速启停**

```bash
ion --host "直接返回 hello 不要调任何工具"
```

验证点：
- [ ] 耗时 < 3 秒
- [ ] 没有残留

**A2-1-3 重复启停不冲突**

```bash
ion --host "做一件事"
ion --host "再做一件事"
```

验证点：
- [ ] 两次独立启动退出
- [ ] 不互相影响
- [ ] 没有僵尸进程

#### Group A2-2：编排执行

**A2-2-1 调度多个子 Worker**

```bash
ion --host "spawn_worker(dev-1, '读 Cargo.toml') + spawn_worker(dev-2, '读 README.md') → await 全部 → 汇总"
```

验证点：
- [ ] coordinator 可 spawn 多个子 Worker
- [ ] 子 Worker 可并行
- [ ] await 等全部完成
- [ ] 汇总后退出

**A2-2-2 带 worktree 隔离**

```bash
ion --host "spawn_worker(dev, '创建一个新文件', worktree='dev/test')"
```

验证点：
- [ ] spawn_worker 带 worktree 参数
- [ ] 子 Worker 在独立 worktree 中执行
- [ ] worktree 被清理

**A2-2-3 coordinator 自己决策**

```bash
ion --host --agent coordinator "分析这个项目结构，拆成模块分配给 Developer"
```

验证点：
- [ ] coordinator 通过 spawn_worker 自己决策
- [ ] 没有硬编码编排逻辑
- [ ] 全部由 prompt 驱动

#### Group A2-3：异步任务

**A2-3-1 基础异步**

```bash
ion --host "spawn_worker(monitor, '监控 /tmp/log') + 继续聊天，等 monitor 完成后汇总"
```

验证点：
- [ ] host 兜着异步子 Worker
- [ ] channel_send 在工作中通信
- [ ] agent_end 被检测
- [ ] 全部 idle 退出

**A2-3-2 多个异步并行**

```bash
ion --host "创建 3 个异步 Worker 分别监控端口 8080/8081/8082"
```

验证点：
- [ ] 多个异步并行跑
- [ ] 各自独立退出
- [ ] channel_send 不混线

#### Group A2-4：退出条件

**A2-4-1 递归退出（两层）**

```bash
ion --host "spawn_worker(A, ...) → A 再 spawn_worker(B, ...)"
```

验证点：
- [ ] Leaf worker 先退 → 父 Worker 后退
- [ ] 递归检测不早杀

**A2-4-2 递归退出（三层）**

```bash
ion --host "spawn_worker(A → B → C)"
```

验证点：
- [ ] 三层嵌套正确退出
- [ ] 按 Leaf → Root 顺序

**A2-4-3 混合同步+异步退出**

```bash
ion --host "先 spawn 一个同步子任务做完，再 spawn 一个异步任务等结果"
```

验证点：
- [ ] 同步任务先退
- [ ] 异步任务继续跑
- [ ] host 等全部 idle 才退
- [ ] 超时兜底

#### Group A2-5：边界与错误

**A2-5-1 spawn 失败（agent 不存在）**

```bash
ion --host "spawn_worker(unknown_agent, '做事')"
```

验证点：
- [ ] agent 不存在时友好报错
- [ ] 不崩溃
- [ ] 主流程继续

**A2-5-2 worker 崩溃不影响 host**

```bash
ion --host "spawn_worker(dev, 'exit 1 进程崩溃')"
```

验证点：
- [ ] 子 Worker 崩溃不影响 host
- [ ] coordinator 能感知
- [ ] 剩余 Worker 不受影响

---

### 场景 3：常驻服务 `ion serve`

#### Group A3-1：服务启停

**A3-1-1 启动服务**

```bash
ion serve
```

预期：

```
🔌 host listening on Unix socket: /Users/xxx/.ion/host.sock
📄 PID: 12345
```

验证点：
- [ ] Unix socket 可用
- [ ] PID 文件写入
- [ ] 进程存活
- [ ] 不自动退出

**A3-1-2 重复启动防冲突**

```bash
# T1 已跑 ion serve
ion serve   # T2 再跑
```

预期：`❌ Host already running (pid 12345)`

验证点：
- [ ] PID 文件防重复启动
- [ ] 提示用 session 或 stop

**A3-1-3 socket 残留自动清理**

```bash
kill -9 <pid> && rm ~/.ion/host.pid
ion serve
```

验证点：
- [ ] 旧 socket 文件被清理
- [ ] 服务正常启动

**A3-1-4 正常停机**

```bash
ion serve stop
```

验证点：
- [ ] 所有 Worker 被 kill
- [ ] socket/pid 文件清理
- [ ] 没有僵尸 Worker

**A3-1-5 服务状态检查**

```bash
ion serve status
```

验证点：
- [ ] 运行时显示 pid/socket/workers/sessions
- [ ] 未运行时显示启动提示

#### Group A3-2：RPC 接入（调试用）

**A3-2-1 create_worker**

```bash
ion rpc --method create_worker \
  --params '{"session":"test","agent":"developer","prompt":"读 Cargo.toml"}'
```

预期：

```json
{
  "type": "response",
  "success": true,
  "data": {"workerId":"wkr_xxx","sessionId":"sess_test"}
}
```

验证点：
- [ ] 通过 RPC 创建 Worker
- [ ] 返回 Worker ID

**A3-2-2 prompt**

```bash
ion rpc --session sess_test --method prompt \
  --params '{"text":"读 Cargo.toml"}'
```

验证点：
- [ ] 同步 prompt 返回 LLM 回复
- [ ] 工具调用结果包含在回复中

**A3-2-3 get_overview**

```bash
ion rpc --method get_overview
```

验证点：
- [ ] 正确报告 Worker 状态

#### Group A3-3：事件订阅

**A3-3-1 subscribe session**

```bash
# T1
ion subscribe --session sess_test
# T2
ion rpc --session sess_test --method prompt --params '{"text":"列出当前目录"}'
```

预期（T1）：

```
{"type":"event","event":{"type":"agent_start"}}
{"type":"event","event":{"type":"text_delta","delta":"正在列出..."}}
{"type":"event","event":{"type":"tool_call","tool":"ls"}}
{"type":"event","event":{"type":"agent_end"}}
```

验证点：
- [ ] subscribe 长连实时推送
- [ ] 完整事件链
- [ ] 断开不影响 Worker

**A3-3-2 断开重连**

验证点：
- [ ] 重连后继续收到后续事件
- [ ] 不因断开丢失 Worker

**A3-3-3 subscribe extension**

```bash
ion subscribe --extension memory
```

验证点：
- [ ] 收到 extension 自定义事件

#### Group A3-4：外部 UI 接入

**A3-4-1 同步任务事件推给 UI**

```bash
# 外部 Web UI 连接 socket 后 subscribe
ion rpc --session sess_ui --method prompt \
  --params '{"text":"spawn_worker(dev, \"写 API\") → await → 汇总"}'
```

预期（外部 UI 收到完整事件链）：

```
agent_start → tool_call(spawn_worker) → agent_start(子) → text_delta → agent_end(子) → agent_end
```

验证点：
- [ ] 同步任务每阶段都推送
- [ ] 子 Worker 事件可见
- [ ] UI 可渲染进度条

**A3-4-2 异步任务卡片**

```bash
ion rpc --session sess_ui --method prompt \
  --params '{"text":"spawn_worker(monitor, \"监控 8080\") → 继续聊天"}'
```

预期（外部 UI 渲染卡片）：

```
┌─ 监控 8080 ─────────┐
│ ● 运行中             │
│ 收到 POST /api       │
│ 响应 200             │
└──────────────────────┘
```

验证点：
- [ ] 异步任务完整生命周期可见
- [ ] channel_send 驱动卡片更新
- [ ] 多卡可独立渲染

#### Group A3-5：多项目管理

**A3-5-1 两项目并行**

```bash
ion rpc --method create_worker \
  --params '{"session":"proj_a","project":"/path/to/a","prompt":"开发认证"}'
ion rpc --method create_worker \
  --params '{"session":"proj_b","project":"/path/to/b","prompt":"开发网关"}'
```

验证点：
- [ ] 同一 host 服务多项目
- [ ] 项目之间 Worker 隔离
- [ ] 事件各自路由

**A3-5-2 跨项目不混**

```bash
ion subscribe --session proj_a
ion subscribe --session proj_b
```

验证点：
- [ ] 各自订阅只收对应事件
- [ ] 不串

#### Group A3-6：边界与错误

**A3-6-1 host 未启动**

```bash
# (没有 ion serve)
ion rpc --method create_worker --params '{}'
```

预期：

```
✘ Host not running. Start it: ion serve
```

验证点：
- [ ] 友好的连接错误

**A3-6-2 session 不存在**

```bash
ion rpc --session nonexistent --method prompt --params '{"text":"hi"}'
```

验证点：
- [ ] 明确报错

**A3-6-3 Worker 崩溃不影响 host**

```bash
ion rpc --session sess_crash --method prompt --params '{"text":"exit 1"}'
```

验证点：
- [ ] Worker 崩溃退出
- [ ] host 不受影响
- [ ] 其他 session 继续

---

## 五、跨场景对比矩阵

| 验证项 | 场景 1 `ion` | 场景 2 `ion --host` | 场景 3 `ion serve` |
|--------|-------------|-------------------|-------------------|
| 单 Agent 执行 | ✅ | ✅ | ✅ |
| 同步子任务 | ✅ 直接 spawn | ✅ 通过 host | ✅ 通过 host |
| 多层嵌套同步 | ✅ | ✅ | ✅ |
| 异步子任务 | ❌ 进程退出被杀 | ✅ host 兜着 | ✅ host 兜着 |
| 多个异步并行 | ❌ | ✅ | ✅ |
| worktree 隔离 | ❌ | ✅ | ✅ |
| channel_send 过程通信 | ❌ 无 host | ✅ | ✅ |
| 事件实时输出 | ❌ | ✅ 事件泵→stdout | ✅ socket→外部 UI |
| 历史事件查看 | ❌ | ❌ | ✅ subscribe 重连 |
| 多会话管理 | ❌ | ❌ | ✅ RPC |
| 多项目并行 | ❌ | ❌ | ✅ |
| 外部 UI 渲染卡片 | ❌ | ❌ | ✅ |
| host 自动清理 | N/A | ✅ 递归 idle 自动关 | ❌ 手动 shutdown |
| 重复启动检测 | N/A | N/A | ✅ PID 文件 |
| socket 残留清理 | N/A | N/A | ✅ 自动 |
| spawn 无 host 时报错 | ✅ | N/A | N/A |

---

## 执行顺序建议

1. **先跑 P0**（修 warnings + unreachable bug）—— 30 分钟，干净起点
2. **再跑 P1**（rename + serve 三件套）—— 2 小时，立即可见成果
3. **核心 P2**（--host 实现）—— 4 小时，是主要工作量
4. **并行 P3**（报错提示）+ **P4**（递归退出）—— 各 1 小时
5. **最后 P5**（文档同步）—— 1 小时收尾

每个 Phase 完成后跑对应的验证 Group，全部 ✅ 才算完成。
