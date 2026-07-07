# CLI 架构设计 — 三场景验证指南

> **状态：设计稿** — 本文定义 ION 的三种 CLI 执行场景、每个场景下的外层 CLI 命令树、分组用例和执行预期，作为落地和验收的依据。

---

## 一、完整 CLI 命令树

```
ion <message>...                            场景 1：快速执行
                                            单进程，跑完即退

ion --host <message>...                     场景 2：快速编排
                                            临时 host，全部 idle 自动关

ion serve                                   场景 3：启动常驻服务
ion serve stop                              场景 3：停止常驻服务
ion serve status                            场景 3：检查服务状态

  ├── session create [--session <id>]       场景 3：创建会话并 prompt
  │              [--agent <name>]
  │              [--model <model>]
  │              [--worktree <branch>]
  │              <message>
  │
  ├── session list                          场景 3：列出所有会话
  │
  ├── session status [<session>]            场景 3：查看会话/全部状态
  │
  ├── session kill <session>                场景 3：停止会话
  │
  ├── session log <session>                 场景 3：查看会话历史事件
  │
  ├── session attach <session>              场景 3：实时订阅事件流
  │                                         类似 tail -f
  │
  └── session send <session> <message>      场景 3：向已有会话发消息
```

> 场景 3 的 `session` 和 `serve` 子命令通过内部 `ion rpc --method ...` 实现，用户不直接打 RPC。

---

## 二、场景对照速查

| | 场景 1 `ion` | 场景 2 `ion --host` | 场景 3 `ion serve` |
|--|-------------|-------------------|-------------------|
| 引擎 | 直接 spawn（无 host） | host 引擎 | host 引擎 + socket |
| 事件出口 | ❌ 无 | 事件泵 → stdout | socket → 外层 CLI |
| 同步子任务 | ✅ spawn→await | ✅ | ✅ |
| 异步任务 | ❌ 进程退出被杀 | ✅ host 兜着 | ✅ host 兜着 |
| 退出方式 | 跑完即退 | 递归 idle 自动关 | 手动 shutdown |
| 多项目 | ❌ | ❌ | ✅ |
| 外部工具接入 | ❌ | ❌ | ✅ 外层 CLI 接管 |

---

## 场景 1：快速执行 `ion "做这个"`

> 单进程单实例。没有 host，没有事件转发。Agent 直接跑，跑完即退。同步子任务可用，异步不可用。

### Group A1-1：基础执行

#### A1-1-1 无参数

```bash
ion
```

**预期：**

```
error: requires a message argument
Usage: ion [OPTIONS] <message>...
```

**验证点：**
- ✅ 友好的错误提示

#### A1-1-2 基本对话

```bash
ion "说 hello"
```

**预期：**

```
Hello! 我是 ION AI 助手...
```

**验证点：**
- ✅ Agent 直接执行，返回回复
- ✅ 没有后台进程残留
- ✅ 进程退出

#### A1-1-3 工具调用

```bash
ion "帮我读 Cargo.toml"
```

**预期：**

```
[调用 read 工具]
[package]
name = "ion"
...
```

**验证点：**
- ✅ Agent 调 read 工具
- ✅ 返回结果
- ✅ 进程退出

---

### Group A1-2：同步子任务

#### A1-2-1 spawn → await（正常）

```bash
ion "调 spawn_worker 创建一个子 Worker 查 Rust 异步编程，await 等他回来，然后汇总"
```

**预期：**

```
正在查 Rust 异步编程...
tokio 是 Rust 的异步运行时...
汇总完成！
```

**验证点：**
- ✅ Agent 调 spawn_worker 创建子 Worker
- ✅ 调 await_worker 等待返回
- ✅ 子 Worker 完成后继续主流程
- ✅ 进程退出

#### A1-2-2 多层 spawn

```bash
ion "调 spawn_worker 创建一个子 Worker A，A 再创建一个子 Worker B"
```

**预期：**

```
A 创建 B...
B 做完了
A 汇总 B 的结果
全部完成
```

**验证点：**
- ✅ 多层嵌套 spawn
- ✅ 每层 await 等回来
- ✅ 进程退出

#### A1-2-3 spawn 后不 await（超时兜底）

```bash
ion "调 spawn_worker 创建一个子 Worker 但不 await 等它，看看会怎样"
```

**预期：**

```
子 Worker 创建成功，但没有 await...
主任务完成，进程退出。
```

**验证点：**
- ✅ spawn 后不 await，主任务完成进程退出
- ❌ 子 Worker 被强杀（没有 host 兜着）
- ✅ 不挂起

---

### Group A1-3：边界与错误

#### A1-3-1 spawn_worker 无 host 时报错

```bash
ion "调 spawn_worker 创建一个子 Worker"
```

**预期：**

```json
{"error": "spawn_worker requires a running host (use --host or ion serve)"}
```

**验证点：**
- ✅ spawn_worker 工具注册了但执行时报错
- ✅ 明确提示用户应该用 --host 或 serve
- ❌ 不会挂起或死锁

#### A1-3-2 channel_send 无 host 时报错

```bash
ion "调 channel_send 发一条消息"
```

**预期：**

```json
{"error": "channel_send requires a running host (use --host or ion serve)"}
```

**验证点：**
- ✅ channel_send 无 host 时报错
- ✅ 不挂起

#### A1-3-3 子 Worker 超时

```bash
ion "调 spawn_worker 创建一个子 Worker 跑 sleep 60，await 等 5 秒超时"
```

**预期：**

```
等待超时，子 Worker 已 kill
继续执行...
```

**验证点：**
- ✅ await_worker 带超时参数
- ✅ 超时后子 Worker 被终止
- ✅ 主流程继续

#### A1-3-4 没有 host 时异步不可用

```bash
ion "spawn_worker 创建一个监控日志的异步任务，然后继续聊天"
```

**预期：**

```
异步任务已创建，但进程退出后它会被杀掉。
建议使用 ion --host 来执行包含异步任务的工作。
```

**验证点：**
- ✅ Agent 知道场景 1 不支持异步
- ✅ 能引导用户使用 --host

---

## 场景 2：快速编排 `ion --host "做这个"`

> 临时启动 host（WorkerRegistry + 事件泵 + 命令循环），所有 Worker idle 后自动关。同步异步都支持。

### Group A2-1：host 启停

#### A2-1-1 host 自动启动

```bash
ion --host "说 hello"
```

**预期：**

```
[host] 启动 WorkerRegistry
[pump] spawning coordinator...
[pump] [wkr_xxx] Hello! 我是 ION AI 助手...
[pump] [wkr_xxx] agent_end
[host] 递归 idle 检测通过
[host] 清理退出
```

**验证点：**
- ✅ host 自动拉起
- ✅ 事件泵实时输出
- ✅ coordinator 执行完后全部 idle
- ✅ host 自动退出

#### A2-1-2 快速启停（无 LLM 调用）

```bash
ion --host "直接返回 hello 不要调任何工具"
```

**预期：**

```
[host] 启动
[pump] [wkr_xxx] hello
[pump] [wkr_xxx] agent_end
[host] 清理退出
==== 耗时 < 3 秒 ====
```

**验证点：**
- ✅ 快速启停，没有残留
- ✅ 第二次运行也能正常启动

#### A2-1-3 重复启停不冲突

```bash
ion --host "做一件事"
ion --host "再做一件事"
```

**验证点：**
- ✅ 两次独立启动退出
- ✅ 不互相影响
- ✅ 没有僵尸进程

---

### Group A2-2：编排执行

#### A2-2-1 调度多个子 Worker

```bash
ion --host "spawn_worker(dev-1, '读 Cargo.toml') + spawn_worker(dev-2, '读 README.md') → await 全部 → 汇总"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(dev-1, "读 Cargo.toml")
[pump] [wkr_b] 读取 Cargo.toml...
[pump] [wkr_a] spawn_worker(dev-2, "读 README.md")
[pump] [wkr_c] 读取 README.md...
[pump] [wkr_b] agent_end
[pump] [wkr_c] agent_end
[pump] [wkr_a] 汇总...
[pump] [wkr_a] agent_end
[host] 清理退出
```

**验证点：**
- ✅ coordinator 可 spawn 多个子 Worker
- ✅ 子 Worker 可并行执行
- ✅ await 等全部完成
- ✅ 汇总后退出

#### A2-2-2 带 worktree 隔离

```bash
ion --host "spawn_worker(dev, '创建一个新文件') + worktree: {branch:'dev/test'} 开发"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(dev, "创建文件", worktree="dev/test")
[pump] [wkr_b] 在 worktree dev/test 中开发...
[pump] [wkr_b] 调 write 工具创建文件...
[pump] [wkr_b] agent_end
[pump] [wkr_a] agent_end
[host] 清理退出
```

**验证点：**
- ✅ spawn_worker 带 worktree 参数
- ✅ 子 Worker 在独立 worktree 中执行
- ✅ worktree 被清理

#### A2-2-3 coordinator 自己决策分配

```bash
ion --host --agent coordinator "分析这个项目结构，拆成模块分配给 Developer"
```

**预期：**

```
[pump] [wkr_c] 分析项目结构...
[pump] [wkr_c] 拆成 3 个模块...
[pump] [wkr_c] spawn_worker(dev, "模块A: 路由层")
[pump] [wkr_c] spawn_worker(dev, "模块B: 数据库层")
[pump] [wkr_c] spawn_worker(dev, "模块C: API 层")
[pump] [wkr_d] 开发模块A...
[pump] [wkr_e] 开发模块B...
[pump] [wkr_f] 开发模块C...
...
[pump] [wkr_d] agent_end
[pump] [wkr_e] agent_end
[pump] [wkr_f] agent_end
[pump] [wkr_c] 全部完成，汇总...
[pump] [wkr_c] agent_end
[host] 清理退出
```

**验证点：**
- ✅ coordinator 通过 spawn_worker 工具自己决策分配
- ✅ 没有硬编码编排逻辑
- ✅ 全部由 prompt 驱动

---

### Group A2-3：异步任务

#### A2-3-1 基础异步

```bash
ion --host "spawn_worker(monitor, '监控 /tmp/log') + 继续聊天，等 monitor 完成后汇总"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(monitor, "监控 /tmp/log")
[pump] [wkr_b] 开始监控 /tmp/log...
[pump] [wkr_a] 我们继续聊点别的...
[pump] [wkr_b] channel_send → "日志文件有更新"
[pump] [wkr_b] channel_send → "又有一条新日志"
[pump] [wkr_b] agent_end
[pump] [wkr_a] 收到监控完成，汇总...
[pump] [wkr_a] agent_end
[host] 清理退出
```

**验证点：**
- ✅ host 兜着异步子 Worker
- ✅ channel_send 在工作过程中通信
- ✅ agent_end 事件被检测到
- ✅ 全部 idle 后退出

#### A2-3-2 多个异步并行

```bash
ion --host "创建 3 个异步 Worker 分别监控端口 8080/8081/8082"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(m1, "监控 8080")
[pump] [wkr_a] spawn_worker(m2, "监控 8081")
[pump] [wkr_a] spawn_worker(m3, "监控 8082")
[pump] [wkr_b] 监控 8080...
[pump] [wkr_c] 监控 8081...
[pump] [wkr_d] 监控 8082...
[pump] [wkr_b] channel_send → "8080 收到请求"
[pump] [wkr_c] agent_end          (8081 无流量，超时退出)
[pump] [wkr_b] agent_end          (8080 处理完)
[pump] [wkr_d] agent_end          (8082 处理完)
[pump] [wkr_a] 三个监控全部完成，汇总...
[pump] [wkr_a] agent_end
[host] 清理退出
```

**验证点：**
- ✅ 多个异步子 Worker 并行跑
- ✅ 各自独立退出
- ✅ channel_send 事件不混线

---

### Group A2-4：退出条件

#### A2-4-1 递归退出（两层）

```bash
ion --host "spawn_worker(A, ...) → A 再 spawn_worker(B, ...)"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(B, "做子任务")
[pump] [wkr_b] 干活...
[pump] [wkr_b] agent_end
[pump] [wkr_a] 完成...
[pump] [wkr_a] agent_end
[host] 递归检测: wkr_a(B ✓) → wkr_b(leaf ✓)
[host] 全部 idle，清理退出
```

**验证点：**
- ✅ Leaf worker 先退 → 父 Worker 后退
- ✅ 递归检测，不会早杀

#### A2-4-2 递归退出（三层）

```bash
ion --host "spawn_worker(A → B → C)"
```

**预期：**

```
[pump] [wkr_c] agent_end  ← C 先退
[pump] [wkr_b] agent_end  ← B 再退
[pump] [wkr_a] agent_end  ← A 最后退
[host] 递归检测: A(B(C ✓) ✓) ✓
[host] 全部 idle，清理退出
```

**验证点：**
- ✅ 三层嵌套正确退出
- ✅ 按 Leaf → Root 顺序

#### A2-4-3 混合同步+异步退出

```bash
ion --host "先 spawn 一个同步子任务做完，再 spawn 一个异步任务等结果"
```

**预期：**

```
[sync] spawn → await → agent_end  (同步立即做完)
[async] spawn → 继续聊天 → ... (异步等着)
→ 用户 ctrl+c 或超时退出
```

**验证点：**
- ✅ 同步任务先退
- ✅ 异步任务继续跑
- ✅ host 一直到全部 idle 才退
- ✅ 超时兜底强制退出

---

### Group A2-5：边界与错误

#### A2-5-1 spawn 失败（agent 不存在）

```bash
ion --host "spawn_worker(unknown_agent, '做事')"
```

**预期：**

```
[pump] [wkr_a] spawn_worker(unknown_agent, "做事")
[pump] [wkr_a] ❌ 错误: agent "unknown_agent" 未定义
```

**验证点：**
- ✅ agent 不存在时友好报错
- ✅ 不崩溃
- ✅ 主流程继续

#### A2-5-2 worker 崩溃不影响 host

```bash
ion --host "spawn_worker(dev, 'exit 1 进程崩溃')"
```

**预期：**

```
[pump] [wkr_b] (进程崩溃退出)
[pump] [wkr_a] 子 Worker 异常退出，已记录
[host] coordinator 检测到子 Worker 异常退出
```

**验证点：**
- ✅ 子 Worker 崩溃不影响 host
- ✅ coordinator 能感知
- ✅ 剩余 Worker 不受影响

---

## 场景 3：常驻服务 `ion serve`

> 启动常驻 host + Unix socket。外部通过 `ion session` / `ion serve` 子命令接入（底层走 RPC）。不自动退出。

### CLI 命令（场景 3 专属）

```bash
ion serve                        # 启动常驻服务
ion serve stop                   # 停止服务
ion serve status                 # 检查服务是否在跑

ion session create [--session <id>] [--agent <name>]   # 创建会话
                   [--model <m>] [--worktree <branch>]
                   <message>
ion session list                 # 列出所有会话
ion session status [<session>]   # 查看会话/全部状态
ion session kill <session>       # 停止会话
ion session log <session>        # 查看历史事件
ion session attach <session>     # 实时订阅（类似 tail -f）
ion session send <session> <message>  # 向会话发消息
```

> 这些子命令内部通过 `ion rpc --method ...` 与 host socket 通信，用户不直接面对 RPC。

---

### Group A3-1：服务启停

#### A3-1-1 启动服务

```bash
ion serve
```

**预期：**

```
🔌 host listening on Unix socket: /Users/xxx/.ion/host.sock
📄 PID: 12345
```

**验证点：**
- ✅ 服务启动，Unix socket 可用
- ✅ PID 文件写入 `~/.ion/host.pid`
- ✅ 进程存活
- ✅ 不自动退出

#### A3-1-2 重复启动防冲突

```bash
# T1 已跑 ion serve
# T2 再跑一次
ion serve
```

**预期：**

```
❌ Host already running (pid 12345). Use `ion session` or `ion serve stop`.
```

**验证点：**
- ✅ PID 文件防重复启动
- ✅ 提示用户使用 session 子命令或停止

#### A3-1-3 socket 残留自动清理

```bash
# kill -9 后重启
ion serve
```

**预期：**

```
🔌 host listening on Unix socket: /Users/xxx/.ion/host.sock
```

**验证点：**
- ✅ 旧 socket 文件被自动清理
- ✅ 服务正常启动

#### A3-1-4 正常停机

```bash
ion serve stop
```

**预期：**

```
Host shutdown complete
```

**验证点：**
- ✅ 所有 Worker 被 kill
- ✅ Unix socket 文件被清理
- ✅ PID 文件被清理
- ✅ 没有僵尸 Worker

#### A3-1-5 服务状态检查

```bash
ion serve status
```

**预期（在跑）：**

```
✔ Host running (pid 12345)
  Socket: /Users/xxx/.ion/host.sock
  Workers: 2
  Sessions: sess_a, sess_b
```

**预期（没跑）：**

```
✘ Host not running
  Start it: ion serve
```

**验证点：**
- ✅ 正确显示运行状态
- ✅ 正确显示未运行

---

### Group A3-2：会话管理

#### A3-2-1 创建会话（同步）

```bash
ion session create --agent developer "读 Cargo.toml"
```

**预期：**

```
✔ Session created: sess_xxx
  Agent: developer
  Model: deepseek-v4-flash
  ─────────────────────────────────
  [package]
  name = "ion"
  ...
```

**验证点：**
- ✅ 创建会话成功
- ✅ 返回 session ID
- ✅ 返回 prompt 结果
- ✖ 会话持续存活（可继续发消息）

#### A3-2-2 创建会话（异步）

```bash
ion session create --agent coordinator "spawn_worker(monitor, '监控 8080') 然后跟我聊天"
```

**预期：**

```
✔ Session created: sess_yyy
  Agent: coordinator
  ─────────────────────────────────
  已创建监控 Worker，我们来聊点别的...
  （会话在后台持续运行）
```

**验证点：**
- ✅ 会话创建后持续存活
- ✅ 异步子 Worker 在后台跑
- ✅ 不自动退出

#### A3-2-3 创建会话带 worktree

```bash
ion session create --agent developer --worktree dev/auth-module "写一个登录模块"
```

**预期：**

```
✔ Session created: sess_zzz
  Worktree: dev/auth-module (created)
  ...
```

**验证点：**
- ✅ worktree 被创建
- ✅ Worker 在 worktree 中工作

#### A3-2-4 会话发送消息

```bash
ion session send sess_xxx "继续，再读一下 README.md"
```

**预期：**

```
  [继续对话]
  README.md 的内容是...
```

**验证点：**
- ✅ 向已有会话发送消息
- ✅ 返回 LLM 回复
- ✅ 会话上下文保持

#### A3-2-5 列出所有会话

```bash
ion session list
```

**预期：**

```
  sess_xxx   developer   running     deepseek-v4-flash    2 min
  sess_yyy   coordinator running    deepseek-v4-flash    5 min
  sess_zzz   developer   idle       deepseek-v4-flash    30 sec
```

**验证点：**
- ✅ 显示所有会话
- ✅ 包含 agent 名、状态、模型、运行时长

#### A3-2-6 查看状态

```bash
ion session status sess_xxx
```

**预期：**

```
  Session:     sess_xxx
  Agent:       developer
  Status:      running
  Model:       deepseek-v4-flash
  Provider:    opencode
  Created:     2 min ago
  Workers:     wkr_a (running)
               ├─ wkr_b (idle)
               └─ wkr_c (running)
  Events:      142 total
```

**验证点：**
- ✅ 显示单个会话详细信息
- ✅ 包含子 Worker 树
- ✅ 包含事件统计

#### A3-2-7 杀死会话

```bash
ion session kill sess_xxx
```

**预期：**

```
✔ Session sess_xxx killed
```

**验证点：**
- ✅ 停止会话
- ✅ 杀死关联的所有 Worker
- ✅ 不影响其他会话

---

### Group A3-3：事件日志

#### A3-3-1 查看历史事件

```bash
ion session log sess_xxx
```

**预期：**

```
  agent_start           2 min ago    ── 开始处理
  text_delta            2 min ago    ── 正在读 Cargo.toml...
  tool_call: read       2 min ago    ── 读文件
  text_delta            2 min ago    ── [package]
  agent_end             2 min ago    ── 完成
```

**验证点：**
- ✅ 显示会话的历史事件
- ✅ 按时间排序
- ✅ 包含关键信息（delta、tool_call）

#### A3-3-2 实时订阅

```bash
# Terminal 1
ion session attach sess_xxx

# Terminal 2
ion session send sess_xxx "列出当前目录"
```

**预期（Terminal 1）：**

```
  [实时事件流]
  agent_start ── 开始处理...
  text_delta  ── 正在列出目录...
  tool_call: ls /Users/xxx/project
  text_delta  ── src/
                  Cargo.toml
                  README.md
  agent_end   ── done
```

**验证点：**
- ✅ attach 后实时推送事件
- ✅ 包含完整事件链
- ✅ ctrl+c 断开不影响 Worker

#### A3-3-3 多会话订阅不混

```bash
# Terminal 1: attach A
ion session attach sess_a

# Terminal 2: attach B
ion session attach sess_b

# 分别发消息
ion session send sess_a "hi"
ion session send sess_b "hello"
```

**验证点：**
- ✅ Terminal 1 只收 sess_a 的事件
- ✅ Terminal 2 只收 sess_b 的事件
- ✅ 不串

---

### Group A3-4：外部 UI 接入

> 外层 CLI `ion session` 提供了完整的命令行接口。外部 UI（Web/TUI/IDE 插件）只需连接到 socket 即可获取相同的事件流。

#### A3-4-1 外部 UI 看到同步任务

```bash
# 外部 UI → socket subscribe session
# 用户通过 CLI 或 Web UI 触发
ion session create --agent developer "spawn_worker(dev, '写 API') → await 完成 → 汇总"
```

**外部 UI 收到的事件：**

```
agent_start
  ├─ tool_call: spawn_worker("dev", "写 API")
  ├─ agent_start(子 Worker)
  ├─ tool_call: write /src/api.rs
  ├─ text_delta: "正在写路由..."
  ├─ text_delta: "写完 GET /users"
  ├─ agent_end(子 Worker)
  ├─ text_delta: "子任务完成，开始汇总"
  └─ agent_end
```

**验证点：**
- ✅ 同步任务的每一阶段都推送给 UI
- ✅ 子 Worker 的事件也可见
- ✅ 外部 UI 可渲染成进度条

#### A3-4-2 外部 UI 看到异步任务卡片

```bash
ion session create --agent coordinator "spawn_worker(monitor, '监控 8080') 然后继续聊天"
```

**外部 UI 收到的事件（渲染成卡片）：**

```
┌─ 监控 8080 ──────────────────────────┐
│ ● 运行中                              │
│  [14:30:01] 开始监听 8080 端口        │
│  [14:32:15] 收到 POST /api/data       │
│  [14:32:15] 响应 200                  │
│  [14:35:00] 收到 GET /health          │
│  [14:35:00] 响应 200                  │
└───────────────────────────────────────┘
↑ 同步更新（channel_send 事件驱动）
```

**验证点：**
- ✅ 异步任务完整生命周期推送
- ✅ channel_send 通信驱动卡片更新
- ✅ 多个异步卡可独立渲染

---

### Group A3-5：多项目管理

#### A3-5-1 两个项目并行

```bash
ion session create --session proj_a --project /path/to/a "开发认证模块"
ion session create --session proj_b --project /path/to/b "开发 API 网关"
```

**验证点：**
- ✅ 同一个 host 同时服务多个项目
- ✅ 项目之间 Worker 隔离
- ✅ 事件各自路由

#### A3-5-2 跨项目事件不混

```bash
ion session attach proj_a    # 只收 A 的事件
ion session attach proj_b    # 只收 B 的事件
ion session send proj_a "继续"
```

**验证点：**
- ✅ 各自订阅只收到对应项目的事件
- ✅ 不串

---

### Group A3-6：边界与错误

#### A3-6-1 host 未启动

```bash
# (没有 ion serve 在跑)
ion session create --agent developer "hi"
```

**预期：**

```
✘ Host not running
  Start it first: ion serve
```

**验证点：**
- ✅ 友好的连接错误提示
- ✅ 指导用户启动服务

#### A3-6-2 session 不存在

```bash
ion session status nonexistent
```

**预期：**

```
✘ Session not found: nonexistent
```

**验证点：**
- ✅ session 不存在时明确报错

#### A3-6-3 Worker 崩溃不影响 host

```bash
# 在一个会话中触发崩溃
ion session send sess_crash "exit 1"
ion session list
```

**验证点：**
- ✅ Worker 崩溃退出
- ✅ host 不受影响
- ✅ 其他会话继续运行
- ✅ host 还在监听

#### A3-6-4 创建会话指定 session ID 冲突

```bash
ion session create --session sess_xxx --agent developer "任务一"
ion session create --session sess_xxx --agent developer "任务二"  # 冲突
```

**预期：**

```
✘ Session already exists: sess_xxx
  Use a different session ID or send: ion session send sess_xxx "..."
```

**验证点：**
- ✅ 重复 session ID 时报错
- ✅ 指导用户使用 session send

---

## 跨场景对比矩阵

| 验证项 | 场景 1 `ion` | 场景 2 `ion --host` | 场景 3 `ion serve` |
|--------|-------------|-------------------|-------------------|
| 单 Agent 执行 | ✅ | ✅ | ✅ |
| 同步子任务 | ✅ 直接 spawn | ✅ 通过 host | ✅ 通过 host |
| 多层嵌套同步 | ✅ | ✅ | ✅ |
| 异步子任务 | ❌ 进程退出被杀 | ✅ host 兜着 | ✅ host 兜着 |
| 多个异步并行 | ❌ | ✅ | ✅ |
| worktree 隔离 | ❌ | ✅ | ✅ |
| channel_send 过程通信 | ❌ 无 host | ✅ | ✅ |
| 事件实时输出 | ❌ | ✅ 事件泵→stdout | ✅ attach 子命令 |
| 历史事件查看 | ❌ | ❌ | ✅ `session log` |
| 多会话管理 | ❌ | ❌ | ✅ `session list/status/kill` |
| 多项目并行 | ❌ | ❌ | ✅ |
| 外部 UI 渲染卡片 | ❌ | ❌ | ✅ socket 推送 |
| host 自动清理 | N/A | ✅ 递归 idle 自动关 | ❌ 手动 shutdown |
| 重复启动检测 | N/A | N/A | ✅ PID 文件 |
| socket 残留清理 | N/A | N/A | ✅ 自动 |
| spawn 无 host 时报错 | ✅ | N/A | N/A |
