# ION CLI 标准用法

> **状态：已验证** — 所有命令均经过 E2E 测试。

## 三大命令

```bash
ion manager start                     # 启动守护进程
ion rpc --method xxx                  # 一问一答（RPC）
ion subscribe --session x             # 实时事件流
```

## 启动 Manager

```bash
# 前台启动（调试用）
ion manager start

# 后台启动
nohup ion manager start > mgr.log 2>&1 &

# 检查是否在跑
ls ~/.ion/manager.pid
```

- Socket：`~/.ion/manager.sock`
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

# Memory 插件
ion rpc --session x --method call_tool \
  --params '{"tool":"memory_save","args":{"content":"偏好 Rust","tags":["rust"]}}'
ion rpc --session x --method call_tool \
  --params '{"tool":"memory_search","args":{"query":"rust"}}'
```

### Plugin RPC（调插件私有方法）

```bash
ion rpc --session x --method plugin_rpc \
  --params '{"method":"ping"}'
# → {"status":"pong","plugin":"memory"}

ion rpc --session x --method plugin_rpc \
  --params '{"method":"save","args":{"content":"...","tags":["a"]}}'

ion rpc --session x --method plugin_rpc \
  --params '{"method":"list","args":{"outline":"preferences"}}'

ion rpc --session x --method plugin_rpc \
  --params '{"method":"search","args":{"query":"rust"}}'

ion rpc --session x --method plugin_rpc \
  --params '{"method":"forget","args":{"id":"mem_1","outline":"auto"}}'

ion rpc --session x --method plugin_rpc \
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

### Plugin subscribe（看插件事件）

```bash
ion subscribe --session sess_xxx --plugin memory
# Ctrl+C 断开
```

收到的事件：

```json
{"type":"subscribed","plugin":"memory","session":"sess_xxx"}
{"type":"plugin_event","plugin":"memory","customType":"memory_saved","data":{"id":"mem_1"}}
{"type":"plugin_event","plugin":"memory","customType":"memory_injected","data":{...}}
```

**用途**：前端记忆面板实时刷新、调试插件行为。

### 通用字段

所有 plugin_event 包含：

| 字段 | 说明 |
|------|------|
| `plugin` | 来源插件 |
| `customType` | 事件类型（插件自定义） |
| `session` | 关联 session |
| `visibility` | `"llm_and_ui"` 或 `"ui_only"` |
| `correlation_id` | 追踪用 |
| `data` | 载荷 |

## 完整调试场景

### 场景 1：开发 + 调试 Memory 插件

```bash
# Terminal 1: Manager
ion manager start

# Terminal 2: 订阅 Memory 事件
ion subscribe --session sess_xxx --plugin memory

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
const socket = connect('unix:' + home + '/.ion/manager.sock');

// 订阅两个流（开两个连接）
const chatStream = connect();
chatStream.send({method:'subscribe', session:'sess_xxx'});
// → 更新聊天面板

const memoryStream = connect();
memoryStream.send({method:'subscribe', plugin:'memory', session:'sess_xxx'});
// → 更新记忆面板

// RPC 调用
const rpcSocket = connect();
rpcSocket.send({method:'call_tool', session:'sess_xxx', params:{tool:'memory_save', args:{...}}});
// → 一问一答
```

## 命令速查

| 命令 | 一句话 |
|------|--------|
| `ion manager start` | 启动 Manager |
| `ion rpc --method list_sessions` | 列会话 |
| `ion rpc --method create_session` | 建会话 |
| `ion rpc --session x --method get_messages` | 读消息 |
| `ion rpc --session x --method call_tool` | 调工具 |
| `ion rpc --session x --method plugin_rpc` | 调插件 |
| `ion rpc --session x --method prompt` | 跑 LLM |
| `ion subscribe --session x` | 看会话流 |
| `ion subscribe --session x --plugin memory` | 看插件事件 |
