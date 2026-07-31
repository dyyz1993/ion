# Monitor Extension v2 设计文档（调度管理 + Scheduler Agent）

> **状态：🔧 设计稿** — v1 已实现（单例扩展 + 基础触发），v2 设计加 `mode` / `trigger_mode` / `max_concurrent` 字段 + Scheduler Agent + 语义校验。代码尚未开始。

---

## 何时使用这个文档

- 给 Monitor Extension 加并发/触发模式字段时
- 设计 Scheduler Agent（专门生成+校验 monitor 的智能体）时
- 实现 GitHub issue / 日志异常 / 进程崩溃等定时监控业务 case 时

**触发时机**：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

**参考样本**：
- [docs/design/MEMORY_AGENT.md](./MEMORY_AGENT.md) — 单例扩展 + Agent 工作流
- [docs/design/HOOKS_AND_OUTLINE_SYNC.md](./HOOKS_AND_OUTLINE_SYNC.md) — 配置驱动的扩展系统

---

## 概览

Monitor Extension 是一个**单例扩展**（只在 `ion serve` 注册），负责定时运行监控脚本，触发条件成立时启动 LLM 对话。v1 已实现基础触发（脚本 stdout 非空 → spawn worker），v2 解决三个核心问题：

1. **并发策略**：上一个 worker 没处理完，下一次触发该怎么办？
2. **消费方接入**：触发后是直接 spawn worker，还是推到 channel，还是只发事件？
3. **配置正确性**：让专门 Agent 生成 monitor.json，并通过 dry-run 保证语义正确

| 能力 | 入口 | 状态 |
|------|------|------|
| 基础触发（脚本 stdout 非空 → spawn worker） | `monitor` 单例 + interval loop | ✅ v1 |
| `mode` 并发策略（serial_skip / serial_queue / concurrent） | `monitor.json#mode` | 🔧 v2 |
| `trigger_mode` 消费方接入（auto_spawn / channel_notify / event_only） | `monitor.json#trigger_mode` | 🔧 v2 |
| `max_concurrent` 并发上限 | `monitor.json#max_concurrent` | 🔧 v2 |
| Scheduler Agent（生成+校验 monitor.json） | `--agent scheduler` | 🔧 v2 |
| `monitor validate` / `monitor test` RPC（dry-run） | `extension_rpc monitor validate/test` | 🔧 v2 |
| Monitor 事件推送（subscribe 可见） | `emit monitor_triggered` | 🔧 v2 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | 单例扩展 + interval loop | ✅ | `monitor_ci.sh A1-A3` |
| 1.2 | `extension_rpc monitor list/add/remove/enable/disable/status` | ✅ | `monitor_ci.sh B1-B3` |
| 1.3 | 空输出不触发 / 错误脚本不崩溃 | ✅ | `monitor_ci.sh C1-C3` |
| 1.4 | 多 monitor 并行加载 | ✅ | `monitor_ci.sh D1` |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |
| 2.X | ✅ 已实现（见 AGENTS.md Monitor Extension v2 段） |

---

## 1. 配置

### 1.1 monitor.json 字段（v2 完整版）

**文件**：`.ion/monitors/<name>.json` 或 `~/.ion/monitors/<name>.json`

```json
{
  "name": "github-issues",
  "interval_secs": 300,
  "script": "gh issue list --repo dyyz1993/ion --state open --label bug --json number,title 2>/dev/null",
  "agent": "developer",
  "prompt_template": "GitHub 有新的 bug issue：\n{output}\n\n请逐一分析并处理。",
  "enabled": true,
  
  "mode": "serial_skip",
  "trigger_mode": "auto_spawn",
  "max_concurrent": 3,
  "cooldown_secs": 60
}
```

**字段说明**：

| 字段 | 类型 | 必填 | 默认 | v1/v2 | 说明 |
|------|------|------|------|-------|------|
| `name` | string | ✅ | — | v1 | 唯一标识，正则 `^[a-zA-Z0-9_-]{1,32}$`（防路径穿越） |
| `interval_secs` | u64 | ✅ | 300 | v1 | 触发间隔（秒），范围 1-86400 |
| `script` | string | ✅ | — | v1 | bash 脚本，exit=0 + stdout 非空 = 触发 |
| `agent` | string | ✅ | "developer" | v1 | 触发的 agent 名（必须已注册） |
| `prompt_template` | string | ❌ | "Monitor triggered:\n{output}" | v1 | prompt 模板，必须含 `{output}` 占位符 |
| `enabled` | bool | ❌ | true | v1 | 是否激活 |
| `mode` | enum | ❌ | "serial_skip" | **v2** | 并发策略（见 1.2） |
| `trigger_mode` | enum | ❌ | "auto_spawn" | **v2** | 消费方接入（见 1.3） |
| `max_concurrent` | u32 | ❌ | 3 | **v2** | concurrent 模式下最大并发数 |
| `cooldown_secs` | u64 | ❌ | 60 | **v2** | 触发后冷却时间（防抖动） |

### 1.2 `mode` 字段：并发策略

| 选项 | 行为 | 适用场景 | 实现复杂度 |
|------|------|---------|----------|
| `serial_skip`（默认） | 找 idle worker → 找不到（全 busy）→ **本轮 skip + emit `monitor_skipped`** | 监控频率高、任务重、不堆积 | 低 |
| `serial_queue` | 找 idle worker → 找不到 → **加入队列，等空闲 worker 触发 `monitor_dequeued`** | 不丢任务、容忍延迟 | 中（队列数据结构） |
| `concurrent` | 每次新建 worker，**最多 max_concurrent 个**，超过则 skip + emit `monitor_throttled` | 任务轻、独立、可并行 | 中（计数器） |

**决策树**（写在 Scheduler Agent prompt 里）：

```
监控对象是什么？
├─ 外部状态拉取（GitHub issue / RSS / 日志）
│   └─ 上次任务可能还没处理完
│       └─ 选 serial_skip（默认推荐）
├─ 周期性巡检（CPU / 磁盘 / 进程存活）
│   └─ 每次都是独立快速检查
│       └─ 选 concurrent + max_concurrent=1
└─ 事件流去重（webhook 转发）
    └─ 不能丢，但可以慢
        └─ 选 serial_queue
```

### 1.3 `trigger_mode` 字段：消费方接入

| 选项 | 行为 | 适用场景 |
|------|------|---------|
| `auto_spawn`（默认） | Monitor 直接 spawn `<agent>` worker，把 prompt_template 渲染后作为 initial_prompt | 全自动、无人值守（CI/自愈） |
| `channel_notify` | Monitor 把消息推到 `main` channel，由**已在跑的 coordinator/developer** 订阅处理 | 复用已有 worker、团队协作 |
| `event_only` | Monitor 只 emit `monitor_triggered` 事件，**不 spawn worker**，subscribe 可见 | 人工接管、调试、审计 |

**决策树**：

```
触发后想怎么处理？
├─ 全自动处理（无人值守）
│   └─ auto_spawn
├─ 已有 coordinator 在跑，让它调度
│   └─ channel_notify
└─ 只想看通知，手动决定
    └─ event_only
```

### 1.4 内核配置

**文件**：[src/monitor_extension.rs](../../src/monitor_extension.rs)

```rust
pub struct MonitorDef {
    // v1
    pub name: String,
    pub interval_secs: u64,
    pub script: String,
    pub agent: String,
    pub prompt_template: String,
    pub enabled: bool,
    
    // v2 新增
    pub mode: MonitorMode,           // serial_skip | serial_queue | concurrent
    pub trigger_mode: TriggerMode,   // auto_spawn | channel_notify | event_only
    pub max_concurrent: u32,
    pub cooldown_secs: u64,
}

pub enum MonitorMode { SerialSkip, SerialQueue, Concurrent }
pub enum TriggerMode { AutoSpawn, ChannelNotify, EventOnly }
```

---

## 2. 主流程 / 数据结构

### 2.1 触发决策流程（v2 核心改造点）

**文件**：`src/monitor_extension.rs`（待改造）

```rust
async fn run_monitor_loop(def: MonitorDef, registry: Arc<...>) {
    let mut interval = tokio::time::interval(Duration::from_secs(def.interval_secs));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    
    // v2: 队列（serial_queue 模式用）
    let pending_queue: VecDeque<String> = VecDeque::new();
    // v2: 当前活跃 worker 数（concurrent 模式用）
    let active_count = Arc::new(AtomicU32::new(0));
    // v2: 上次触发时间（cooldown 用）
    let last_trigger = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(def.cooldown_secs)));
    
    loop {
        interval.tick().await;
        
        // Step 1: 运行脚本
        let (ok, output) = Self::run_script(&def.script);
        if !ok { emit("monitor_script_failed", ...); continue; }
        if output.is_empty() { continue; }  // 空输出不触发
        
        // Step 2: cooldown 检查
        if last_trigger.lock().await.elapsed() < Duration::from_secs(def.cooldown_secs) {
            emit("monitor_cooldown", ...);
            continue;
        }
        
        // Step 3: 按 trigger_mode 决定动作
        match def.trigger_mode {
            TriggerMode::EventOnly => {
                emit("monitor_triggered", {output, mode, trigger_mode});
                continue;  // 不 spawn worker
            }
            TriggerMode::ChannelNotify => {
                registry.send_to_channel("main", &prompt).await;
                emit("monitor_notified_channel", ...);
                continue;
            }
            TriggerMode::AutoSpawn => {
                // Step 4: 按 mode 决定并发策略
                match def.mode {
                    SerialSkip => {
                        if let Some(idle) = find_idle_worker(&def.agent) {
                            send_prompt(idle, &prompt);
                        } else {
                            emit("monitor_skipped", {reason: "all_busy"});
                        }
                    }
                    SerialQueue => {
                        if let Some(idle) = find_idle_worker(&def.agent) {
                            // 先处理队列里旧的
                            if let Some(queued) = pending_queue.pop_front() {
                                send_prompt(idle, &queued);
                            } else {
                                send_prompt(idle, &prompt);
                            }
                        } else {
                            pending_queue.push_back(prompt);
                            emit("monitor_queued", {queue_len: pending_queue.len()});
                        }
                    }
                    Concurrent => {
                        if active_count.load() < def.max_concurrent {
                            let worker = create_worker(&def.agent, &prompt);
                            active_count.fetch_add(1);
                            // worker 完成时回调 active_count.fetch_sub(1)
                            emit("monitor_spawned", {worker_id: worker});
                        } else {
                            emit("monitor_throttled", {active: active_count.load()});
                        }
                    }
                }
            }
        }
        
        *last_trigger.lock().await = Instant::now();
    }
}
```

### 2.2 关键决策点

| 场景 | 处理 |
|------|------|
| 上一个 worker 还在跑（serial_skip） | skip 本轮 + emit `monitor_skipped` |
| 队列积压超过 10 条（serial_queue） | emit `monitor_queue_overflow` + 丢弃最旧 |
| 并发达到上限（concurrent） | emit `monitor_throttled` |
| cooldown 内重复触发 | emit `monitor_cooldown` + skip |
| 脚本失败（exit≠0） | emit `monitor_script_failed` + 继续下一轮 |
| 脚本成功但 stdout 为空 | 静默（不算触发） |
| `trigger_mode=event_only` | 只 emit `monitor_triggered`，不 spawn worker |
| `trigger_mode=channel_notify` 但 main channel 无订阅者 | emit `monitor_no_subscriber` + 退化为 event_only |
| agent 名不存在 | emit `monitor_agent_not_found` + 自动 disable 该 monitor |

### 2.3 Scheduler Agent 工作流

**文件**：`examples/agents/scheduler.md`（待创建）

Scheduler Agent 是一个**纯 prompt agent**（无代码校验，先看 prompt 能不能搞定 100% 正确）。它的工作流写在 prompt 里：

```
用户："我要监控 GitHub issue"
   ↓
Scheduler Agent 启动
   ↓ Step 1: 澄清需求
"监控哪个 repo？多久检查一次？有新 issue 怎么处理？"
   ↓ Step 2: 决策 mode/trigger_mode
（按 1.2/1.3 决策树）
   ↓ Step 3: 写 monitor.json
（按模板 + 例子）
   ↓ Step 4: 调 monitor test dry-run
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"test","params":{...}}'
   ↓ Step 5: 通过则调 monitor add
   ↓ Step 6: emit scheduler_installed 事件
```

---

## 3. 关键 bug fix 记录

> v2 还没实现，先记录 v1 已踩过的坑。

### Bug 1：路径穿越（v2 必须修）

**文件**：`src/monitor_extension.rs:329-333`

**修复前**：

```rust
let path = monitor_dir.join(format!("{name}.json"));  // name 直接拼接
```

如果 agent 传 `name: "../../../etc/cron.d/evil"`，会写到任意路径。

**修复后**：

```rust
// name 必须匹配 ^[a-zA-Z0-9_-]{1,32}$
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!("name 长度必须在 1-32 之间，当前 {}", name.len()));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!("name 只允许 [a-zA-Z0-9_-]，当前 '{name}'"));
    }
    Ok(())
}
```

### Bug 2：interval_secs=0 导致死循环（v2 必须修）

**修复前**：`tokio::time::interval(Duration::from_secs(0))` 立即返回，CPU 100%。

**修复后**：`validate()` 强制 `interval_secs >= 1`。

### Bug 3：add 同名静默覆盖（v2 必须修）

**修复前**：

```rust
monitors.retain(|m| m.name != def.name);  // 静默删除
monitors.push(def.clone());                // 再 push
```

agent 不知道自己覆盖了已有 monitor。

**修复后**：同名时返回 `{"success": false, "error": "monitor '{name}' already exists, use 'update' instead"}`。

---

## 4. 接口规格

### 4.1 `monitor list` — 列出所有 monitor

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"list"}'
```

**响应 JSON（成功）**：

```json
{
  "type": "response",
  "id": "1",
  "success": true,
  "data": {
    "monitors": [
      {
        "name": "github-issues",
        "interval_secs": 300,
        "agent": "developer",
        "enabled": true,
        "mode": "serial_skip",
        "trigger_mode": "auto_spawn",
        "max_concurrent": 3,
        "trigger_count": 12,
        "last_run": "2026-07-26T10:30:00Z",
        "last_result": "triggered",
        "active_workers": 0,
        "queue_length": 0
      }
    ]
  }
}
```

### 4.2 `monitor add` — 添加 monitor（v2 强制 validate）

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "add",
    "params": {
      "name": "github-issues",
      "interval_secs": 300,
      "script": "gh issue list --repo dyyz1993/ion --state open --label bug 2>/dev/null",
      "agent": "developer",
      "prompt_template": "New bug issues:\n{output}\nProcess them.",
      "mode": "serial_skip",
      "trigger_mode": "auto_spawn"
    }
  }'
```

**响应 JSON（成功）**：

```json
{
  "type": "response",
  "success": true,
  "data": {
    "added": "github-issues",
    "validated": true,
    "file": ".ion/monitors/github-issues.json"
  }
}
```

**响应 JSON（校验失败）**：

```json
{
  "type": "response",
  "success": false,
  "error": "monitor validation failed",
  "data": {
    "errors": [
      "interval_secs=0 不合法，必须 >= 1",
      "script 语法错误：line 1: syntax error near unexpected token `('",
      "agent 'foobar' 不存在，可选：build/developer/reviewer/coordinator"
    ]
  }
}
```

### 4.3 `monitor validate` — 仅校验不落盘（v2 新增）

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "validate",
    "params": { ... 同 add 的 def ... }
  }'
```

**响应 JSON（通过）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "warnings": [
      "interval_secs=86400 可能太长，建议 300-3600"
    ]
  }
}
```

**响应 JSON（不通过）**：

```json
{
  "success": true,
  "data": {
    "valid": false,
    "errors": [
      "name 含非法字符：'github issues'（只允许 [a-zA-Z0-9_-]）",
      "prompt_template 缺少 {output} 占位符"
    ]
  }
}
```

### 4.4 `monitor test` — dry-run 试跑（v2 新增）

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{
    "extension": "monitor",
    "method": "test",
    "params": {
      "script": "gh issue list --repo dyyz1993/ion --state open --label bug 2>/dev/null",
      "prompt_template": "Issues:\n{output}"
    }
  }'
```

**响应 JSON（成功）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "script_exit_ok": true,
    "script_stdout": "[{\"number\":42,\"title\":\"fix: ...\"}]",
    "script_stderr": "",
    "would_trigger": true,
    "rendered_prompt": "Issues:\n[{\"number\":42,\"title\":\"fix: ...\"}]"
  }
}
```

**响应 JSON（脚本失败）**：

```json
{
  "success": true,
  "data": {
    "valid": true,
    "script_exit_ok": false,
    "script_exit_code": 127,
    "script_stderr": "gh: command not found",
    "would_trigger": false
  }
}
```

### 4.5 `monitor status` — 查询运行时状态

**请求**：

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"monitor","method":"status"}'
```

**响应 JSON**：

```json
{
  "success": true,
  "data": {
    "statuses": [
      {
        "name": "github-issues",
        "trigger_count": 12,
        "skip_count": 3,
        "queue_length": 0,
        "active_workers": 0,
        "last_run": "2026-07-26T10:30:00Z",
        "last_result": "triggered",
        "last_error": null
      }
    ]
  }
}
```

### 4.6 其他 RPC（v1 已实现）

| 方法 | 行为 |
|------|------|
| `monitor remove --name X` | 删除 monitor + 删 .json 文件 |
| `monitor enable --name X` | 激活 |
| `monitor disable --name X` | 停用（保留配置） |

---

## 5. CLI 测试指南

> 详细测试 case 见独立的 [MONITOR_CLI_TEST.md](../testing/MONITOR_CLI_TEST.md)。

### 测试组概览（按用户场景分）

| Group | 场景 | case 数 |
|-------|------|--------|
| **A** 基础加载+触发 | monitor 能加载、能跑、能触发 | 5 |
| **B** RPC 管理 | list/add/remove/enable/disable | 6 |
| **C** 空输出+错误处理 | 不该触发的不触发 | 4 |
| **D** 多 monitor 并行 | 多个 monitor 同时跑 | 3 |
| **E** 并发策略（v2） | serial_skip / serial_queue / concurrent | 9 |
| **F** 消费方接入（v2） | auto_spawn / channel_notify / event_only | 6 |
| **G** Scheduler Agent（v2） | .md agent 生成 + 校验 + dry-run | 5 |
| **H** 事件订阅 | subscribe 看 monitor_* 事件 | 5 |
| **I** 业务场景 | GitHub issue / 日志 / 进程崩溃 | 4 |
| **J** 边界+安全 | 路径穿越 / 越界 / 注入 | 6 |
| **合计** | | **53** |

---

## 6. 后续工作

| # | 待办 | 优先级 | 依赖 |
|---|------|--------|------|
| 1 | 实现 v2 字段（mode/trigger_mode/max_concurrent/cooldown） | P0 | — |
| 2 | 实现 `monitor validate` + `monitor test` RPC | P0 | 1 |
| 3 | 写 `examples/agents/scheduler.md` | P0 | 1, 2 |
| 4 | 写 `docs/testing/MONITOR_CLI_TEST.md`（53 case） | P0 | — |
| 5 | 写 `tests/monitor_ci.sh` v2（Group E-J） | P1 | 1-4 |
| 6 | 端到端业务 case（GitHub issue 闭环） | P1 | 1-5 |
| 7 | 同样的模式套到 workflow（workflow-author agent + workflow.validate 语义校验） | P2 | — |

---

## 附录 A：业务 case 蓝图（部分已实现，见 AGENTS.md）

### A1：GitHub issue 定时拉取 → 自动处理

```bash
# 用 scheduler agent 一句话生成
ion --agent scheduler "监控 https://github.com/dyyz1993/ion 的新 bug issue，每 5 分钟检查一次，有新 issue 自动让 developer 处理"

# 生成的 .ion/monitors/github-issues.json：
{
  "name": "github-issues",
  "interval_secs": 300,
  "script": "gh issue list --repo dyyz1993/ion --state open --label bug --json number,title 2>/dev/null | jq 'length | tostring'",
  "agent": "developer",
  "mode": "serial_skip",
  "trigger_mode": "auto_spawn",
  "prompt_template": "有 {output} 个新 bug issue，请用 gh issue view 逐一查看并处理"
}

# 启动
ion serve

# 5 分钟后 monitor 触发 → spawn developer worker → 处理 issue
# subscribe 可见事件流：
#   monitor_triggered {output: "3", mode: "serial_skip"}
#   monitor_spawned {worker_id: "wkr_xxx", agent: "developer"}
#   agent_start / text_delta / agent_end（developer 处理过程）
```

### A2：日志异常监控 → 通知 coordinator

```json
{
  "name": "error-log-scan",
  "interval_secs": 60,
  "script": "grep -E 'ERROR|panic' /var/log/myapp.log 2>/dev/null | tail -5",
  "agent": "coordinator",
  "mode": "serial_queue",
  "trigger_mode": "channel_notify",
  "prompt_template": "发现日志异常：\n{output}\n请协调相关 worker 排查"
}
```

### A3：进程存活检查 → 只通知不处理

```json
{
  "name": "process-alive",
  "interval_secs": 30,
  "script": "pgrep -f 'critical-service' || echo 'CRITICAL_DOWN'",
  "agent": "user",
  "mode": "concurrent",
  "max_concurrent": 1,
  "trigger_mode": "event_only",
  "prompt_template": "关键进程挂了：{output}"
}
```

### A4：周期性巡检 → 并发处理

```json
{
  "name": "disk-usage",
  "interval_secs": 600,
  "script": "df -h | awk '$5+0 > 80 {print $5\" \"$6}'",
  "agent": "maintainer",
  "mode": "concurrent",
  "max_concurrent": 3,
  "trigger_mode": "auto_spawn",
  "prompt_template": "磁盘使用率告警：\n{output}\n请清理"
}
```
