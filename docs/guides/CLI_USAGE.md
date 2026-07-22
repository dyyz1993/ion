# ION CLI 标准用法

> **状态：已验证** — 所有命令均经过 E2E 测试。

## 三大命令

```bash
ion serve start                     # 启动守护进程
ion rpc --method xxx                  # 一问一答（RPC）
ion subscribe --session x             # 实时事件流
```

## 快速执行（直接跑 prompt）

最常用的入口——不走 host，直接 spawn 子进程跑完即退：

```bash
# 基本用法（用 config.json 的 default-model）
ion "帮我读一下 Cargo.toml"

# 非交互模式（-p/--print）：跑完直接输出结果，适合脚本/CI
ion -p "say hello in 3 words"

# 指定 provider + model（临时覆盖，不改 config）
ion -p "写一个 hello world" --provider opencode --model deepseek-v4-flash

# 用 anthropic 的 claude
ion -p "分析这段代码" --provider anthropic --model claude-opus-4-8

# 多行输入（stdin 管道自动检测）
echo "解释这个错误" | ion -p

# 指定 agent（build/explore/plan 或自定义 .md）
ion -p "重构这个模块" --agent explore
```

**flag 说明**：

| flag | 说明 | 示例 |
|------|------|------|
| `-p` / `--print` | 非交互模式，跑完即退（对齐 pi） | `ion -p "hello"` |
| `--provider` | Provider 名（opencode/anthropic/openai/zhipuai/deepseek…） | `--provider opencode` |
| `--model` | 模型 ID（deepseek-v4-flash/glm-4.7/claude-opus-4-8…） | `--model deepseek-v4-flash` |
| `--agent` | 使用命名 agent（build/explore/plan 或 .md 路径） | `--agent build` |

> **测试/开发推荐**：`--provider opencode --model deepseek-v4-flash`——便宜、快速、够用。真实 LLM 测试时优先用这个组合，避免用昂贵的模型。

## 启动 Host

```bash
# 前台启动（调试用）
ion serve start

# 后台启动
nohup ion serve start > mgr.log 2>&1 &

# 检查是否在跑
ls ~/.ion/manager.pid
```

- Socket：`~/.ion/host.sock`
- PID：`~/.ion/manager.pid`（防重复启动）
- 关闭：`kill $(cat ~/.ion/manager.pid)`

## RPC（一问一答）

### Manager 级（不指定 session）

```bash
ion rpc --method list_sessions
# → {"sessions":[{"session_id":"sess_xxx","agent":"developer",...}]}

ion rpc --method create_session --params '{"agent":"developer"}'
# → {"session_id":"sess_xxx","status":"created"}

ion rpc --method list_workers
# → 列出所有 worker（含内部 worker_id）

ion rpc --method create_worker --params '{...}'
# → 直接创建 worker + 注入 initial_prompt

ion rpc --method kill --params '{"workerId":"wkr_xxx"}'
```

### Instance 级（查 Worker 状态）

```bash
ion rpc --session sess_xxx --method get_messages
# → 该 session 的所有消息

ion rpc --session sess_xxx --method get_state
# → {"model":"glm-4.7","provider":"zhipuai","agent":"developer",...}

ion rpc --session sess_xxx --method get_tools
# → 该 session 注册的所有工具
```

### Tool RPC（调 LLM 工具）

```bash
# 文件类
ion rpc --session x --method call_tool \
  --params '{"tool":"read","args":{"path":"Cargo.toml"}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"ls","args":{"path":"."}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"write","args":{"path":"a.rs","content":"fn main(){}"}}'

# Bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash","args":{"command":"git status"}}'

# Worker 编排
ion rpc --session x --method call_tool \
  --params '{"tool":"spawn_worker","args":{"relation":"child","agent":"developer","task":"...","wait":true}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"send_to_worker","args":{"worker_id":"wkr_xxx","text":"hi"}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"resume_worker","args":{"worker_id":"wkr_xxx","text":"继续"}}'

# Memory 扩展
ion rpc --session x --method call_tool \
  --params '{"tool":"memory_save","args":{"content":"偏好 Rust","tags":["rust"]}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"memory_search","args":{"query":"rust"}}'
```

### Extension RPC（调扩展私有方法）

```bash
ion rpc --session x --method extension_rpc \
  --params '{"method":"ping"}'
# → {"status":"pong","extension":"memory"}

ion rpc --session x --method extension_rpc \
  --params '{"method":"save","args":{"content":"...","tags":["a"]}}'

ion rpc --session x --method extension_rpc \
  --params '{"method":"list","args":{"outline":"preferences"}}'

ion rpc --session x --method extension_rpc \
  --params '{"method":"search","args":{"query":"rust"}}'

ion rpc --session x --method extension_rpc \
  --params '{"method":"forget","args":{"id":"mem_1","outline":"auto"}}'

ion rpc --session x --method extension_rpc \
  --params '{"method":"inspect","args":{"id":"mem_1"}}'
```

### Prompt（让 LLM 执行）

```bash
# 空闲时直接执行
ion rpc --session x --method prompt \
  --params '{"text":"用 memory_save 记住我喜欢 Rust"}'

# 忙时打断
ion rpc --session x --method prompt \
  --params '{"text":"停下来","behavior":"interrupt"}'

# 忙时排队到 steering 队列
ion rpc --session x --method prompt \
  --params '{"text":"补充一下","behavior":"steer"}'

# 忙时排队到 follow_up 队列
ion rpc --session x --method prompt \
  --params '{"text":"稍后处理","behavior":"followUp"}'
```

### 其他 RPC

```bash
# abort（硬终止当前 Agent）
ion rpc --session x --method abort

# steer（注入 steering 队列 + 可选打断）
ion rpc --session x --method steer \
  --params '{"text":"改用 TypeScript","immediate":true}'
ion rpc --session x --method steer \
  --params '{"promote":0}'  # 提权 follow_up[0] 到 steering

# follow_up（排到 Agent 当前任务后面）
ion rpc --session x --method follow_up \
  --params '{"text":"顺便加个注释"}'

# abort → 硬停止
ion rpc --session x --method abort
```

## Subscribe（实时事件流）

### Instance subscribe（看会话流）

```bash
ion subscribe --session sess_xxx
# Ctrl+C 断开
```

收到的事件：

```json
{"type":"subscribed","session":"sess_xxx","stream":"instance"}
{"type":"instance_event","event":{"type":"agent_start","sessionId":"sess_xxx"}}
{"type":"instance_event","event":{"type":"text_delta","delta":"正在思考..."}}
{"type":"instance_event","event":{"type":"tool_call","tool":"memory_save"}}
{"type":"instance_event","event":{"type":"agent_end","sessionId":"sess_xxx"}}
```

**用途**：实时看 LLM 输出、调试 Agent 行为、前端聊天面板。

### Extension subscribe（看扩展事件）

```bash
ion subscribe --session sess_xxx --extension memory
# Ctrl+C 断开
```

收到的事件：

```json
{"type":"subscribed","extension":"memory","session":"sess_xxx"}
{"type":"extension_event","extension":"memory","customType":"memory_saved","data":{"id":"mem_1"}}
{"type":"extension_event","extension":"memory","customType":"memory_injected","data":{...}}
```

**用途**：前端记忆面板实时刷新、调试扩展行为。

### 通用字段

所有 extension_event 包含：

| 字段 | 说明 |
|------|------|
| `extension` | 来源扩展 |
| `customType` | 事件类型（扩展自定义） |
| `session` | 关联 session |
| `visibility` | `"llm_and_ui"` 或 `"ui_only"` |
| `correlation_id` | 追踪用 |
| `data` | 载荷 |

## 完整调试场景

### 场景 1：开发 + 调试 Memory 扩展

```bash
# Terminal 1: Manager
ion serve start

# Terminal 2: 订阅 Memory 事件
ion subscribe --session sess_xxx --extension memory

# Terminal 3: 操作
ion rpc --method create_session --params '{"agent":"developer"}'
# → sess_xxx

# RPC 直调验证（不经过 LLM）
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_save","args":{"content":"偏好 Rust","tags":["rust"]}}'
# → Terminal 2 收到 memory_saved

# LLM 引导
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"请记住我喜欢 TypeScript"}'
# → Terminal 2 收到 memory_saved（LLM 调了 memory_save）

# RPC 佐证
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_search","args":{"query":"语言"}}'
# → 返回所有匹配的记忆
```

### 场景 2：调试 Worker 编排

```bash
# Terminal 1: 订阅会话流
ion subscribe --session sess_xxx
# → 实时看到 agent_start → text_delta → tool_call(spawn_worker) → agent_end

# Terminal 2: 触发
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"spawn_worker","args":{"relation":"child","agent":"developer","task":"创建 a.rs","wait":true}}'
```

### 场景 3：前端集成

```javascript
// 连接 Manager socket（伪代码）
const socket = connect('unix:' + home + '/.ion/host.sock');

// 订阅两个流（开两个连接）
const chatStream = connect();
chatStream.send({method:'subscribe', session:'sess_xxx'});
// → 更新聊天面板

const memoryStream = connect();
memoryStream.send({method:'subscribe', extension:'memory', session:'sess_xxx'});
// → 更新记忆面板

// RPC 调用
const rpcSocket = connect();
rpcSocket.send({method:'call_tool', session:'sess_xxx', params:{tool:'memory_save', args:{...}}});
// → 一问一答
```

## 命令速查

| 命令 | 一句话 |
|------|--------|
| `ion serve start` | 启动 Host |
| `ion rpc --method list_sessions` | 列会话 |
| `ion rpc --method create_session` | 建会话 |
| `ion rpc --session x --method get_messages` | 读消息 |
| `ion rpc --session x --method call_tool` | 调工具 |
| `ion rpc --session x --method extension_rpc` | 调扩展 |
| `ion rpc --session x --method prompt` | 跑 LLM |
| `ion subscribe --session x` | 看会话流 |
| `ion subscribe --session x --extension memory` | 看扩展事件 |

## 会话列表（ion sessions）

> 按主仓库维度查询会话——当前 cwd 所在 git 仓库的所有会话（含 worktree）。

### 基本用法

```bash
# 当前主仓库的会话（默认行为，自动聚合 worktree）
ion sessions

# 所有项目的会话（关闭过滤）
ion sessions --all

# 限制显示条数（表格模式，默认 20）
ion sessions --limit 5

# JSON 输出（脚本/UI 消费）
ion sessions --json

# 组合
ion sessions --json --limit 10
ion sessions --all --json
```

### 表格输出字段

| 列 | 说明 |
|---|---|
| ID | 会话 ID（截断显示）|
| AGENT | Agent 名称（build/developer/coordinator…）|
| MODEL | 模型 ID |
| BRANCH | 创建时的 git 分支 |
| MSGS | 消息数 |
| TOKENS(IN/OUT/CA) | 输入/输出/缓存 token（缓存 = cache_read + cache_write）|
| CREATED | 创建时间（相对，如 `2d ago`）|
| UPDATED | 最后更新时间 |
| WT | `🌿` = worktree 会话 |

### JSON 输出字段

```json
{
  "project": {"cwd": "...", "projectKey": "ee768d6fc5459315"},
  "sessions": [{
    "id": "sess_xxx",
    "name": null,
    "project": "/abs/path",
    "projectName": "ion",
    "worktree": true,
    "branch": "master",
    "model": "glm-4.7",
    "agent": "default",
    "provider": "zhipuai",
    "createdAt": 1783771530267,
    "updatedAt": 1783771535442,
    "messageCount": 19,
    "turnCount": 9,
    "tokenInput": 8670,
    "tokenOutput": 302,
    "tokenCacheRead": 0,
    "tokenCacheWrite": 0,
    "parentSession": null,
    "thinkingLevel": null
  }],
  "totalCount": 25
}
```

> `--all` 时 `project` 为 `null`。

### 主仓库聚合原理

过滤逻辑：对当前 cwd 和每个历史会话的 `project` 路径分别调 `paths::project_key_git()`（基于 `git rev-parse --git-common-dir`），**key 相同的会话才显示**。

- 主仓库 cwd 和 worktree cwd 算出同一个 key → worktree 会话被正确归入主仓库
- 非 git 目录的旧会话（key 退化成 cwd hash）→ 与当前主仓库 key 不同 → 自动过滤
- `project` 目录已删除的会话 → git 调用失败 → 自动过滤

> 相关设计：[CONFIG_DIMENSIONS.md §2.4](../design/CONFIG_DIMENSIONS.md) project_key 算法。

### 脚本示例

```bash
# 统计当前项目总 token 消耗
ion sessions --json | jq '[.sessions[].tokenInput] | add'

# 列出所有 worktree 会话
ion sessions --json | jq '.sessions[] | select(.worktree) | {id, branch}'

# 全局 token 消耗（所有项目）
ion sessions --all --json | jq '[.sessions[] | .tokenInput + .tokenOutput] | add'
```

### 与 RPC 的区别

| 命令 | 数据源 | 范围 |
|---|---|---|
| `ion sessions` | 磁盘索引 `sessions.index.json` | 当前主仓库（或 `--all` 全部）|
| `ion rpc --method list_sessions` | 内存 Worker | 当前运行中的 Worker |
| `ion rpc --method list_all_sessions` | 磁盘索引 | 全部会话（无过滤，含血缘字段）|

## Session Tree（会话分支）

> 让一个会话内部能形成树——回退到任意消息分叉，原路径保留。

### 分叉

```bash
# 从某条消息分叉
ion --resume <sid> --branch <entry-id> "新指令"

# 分叉并命名
ion --resume <sid> --branch <entry-id> --branch-name try-async "用 async 重写"
```

### 回滚

```bash
# 回滚（被回滚的路径保留）
ion --resume <sid> --rollback <entry-id> "回滚后继续"

# 带 tombstone（记录原因）
ion --resume <sid> --rollback <entry-id> --rollback-reason "方案走错了" "继续"
```

### 切换分支

```bash
ion --resume <sid> --checkout try-async "切到 async 分支"
```

### 从分支点提取新 session

```bash
ion --fork-from-leaf <sid>/<entry-id> "在新 session 继续"
```

### 查看树/分支

```bash
ion session tree <sid>        # ASCII 树展示
ion session branches <sid>    # 命名分支列表
```

### Agent 工具

Agent 自主调用 `branch_session` 工具分叉/回滚（参数：from_entry / name / is_rollback / reason）。

## FauxProvider & Record/Replay（LLM Mock + 录制回放）

> 不调真实 LLM 的测试/开发工具。

### FauxProvider（手写响应）

```bash
# 单条预设回复
ION_FAUX_REPLY="hello from faux" ion "say hi"

# 脚本文件（多步响应）
cat > /tmp/script.jsonl <<'EOF'
{"text":"我先读文件"}
{"tool_call":{"name":"read","input":{"path":"Cargo.toml"}}}
{"text":"读完了"}
EOF
ION_FAUX_SCRIPT=/tmp/script.jsonl ion "分析项目"

# host 模式同样适用
ION_FAUX_REPLY="host reply" ion --host "编排任务"
```

### Record/Replay（录制真实会话 + 回放）

```bash
# 录制（正常用，自动存到 ~/.ion/recordings/<id>/）
ION_RECORD=fix-bug-2026 ion --model glm-4.6 "修复 bug"

# 回放（不联网，免 API key）
ion --model replay/fix-bug-2026 "修复 bug"

# 覆盖已有录制
ION_RECORD=fix-bug-2026 ION_RECORD_OVERWRITE=1 ion --model glm-4.6 "重录"

# 列出所有录制
ion recordings
```

### 录制脚本格式

```jsonl
{"text":"纯文本回复"}
{"tool_call":{"name":"read","input":{"path":"x"}}}
{"thinking":"先想想","text":"然后回答"}
{"text":"","stop_reason":"error","error_message":"模拟错误"}
```

## Multi-Agent Orchestration

### spawn_worker (sync child)
```bash
# Scenario 1: coordinator spawns developer synchronously
ion --host --agent coordinator "Add a method to src/auth.rs"
# coordinator internally calls: spawn_worker(child, developer, wait=true)
```

### spawn_worker (async, parallel)
```bash
# Scenario 2: coordinator spawns 3 developers in parallel
ion --host --agent coordinator "Fix 3 independent bugs in auth.rs, paths.rs, tool.rs"
# coordinator internally calls:
#   spawn_worker(child, developer, wait=false) x 3
#   await_worker(worker_1)
#   await_worker(worker_2)
#   await_worker(worker_3)
```

### spawn_worker (peer, background)
```bash
# Scenario 3: coordinator spawns background reviewer
# coordinator internally calls: spawn_worker(peer, reviewer, report_channel='main')
# reviewer auto-reports via follow_up when done
```

### Tool Summary

| Tool | Type | When to use |
|------|------|-------------|
| spawn_worker(child, wait=true) | Sync | Single task, must wait |
| spawn_worker(child, wait=false) | Async | Parallel tasks |
| spawn_worker(peer) | Async | Background/monitoring |
| resume_worker | Sync | Continue conversation with completed worker |
| send_to_worker | Async | Message to background worker |
| await_worker | Sync | Wait for async worker |
| kill_worker | Async | Terminate stuck worker |
| channel_send | Async | Broadcast to all workers |

## Self-Evolution (A→B Architecture)

### Quick Start
```bash
# 1. Start container + compile
bash scripts/evolve.sh

# 2. Run self-evolution task (B writes code, A merges)
bash scripts/evolve_pr.sh "Add fn count_active() to src/global_memory.rs"

# 3. Or run batch tasks
bash scripts/evolve_self.sh
```

### How It Works
- A = coordinator agent on host (never writes code directly)
- B = developer agent in container (writes code + tests + commits)
- 6 gate checks: U+FFFD, Cargo.toml, reviewer, cargo build, cargo test, clippy
- Changes managed via GitHub PR (evolve_pr.sh)

### Key Scripts
| Script | Purpose |
|--------|---------|
| evolve.sh | Start container + compile ion/ion-worker |
| evolve_self.sh | Serial batch tasks |
| evolve_concurrent.sh | 1 container + N parallel B workers |
| evolve_native.sh | Coordinator uses spawn_worker natively |
| evolve_pr.sh | B writes → gate → GitHub PR → merge |
| evolve_verify.sh | Standalone CI verification |
