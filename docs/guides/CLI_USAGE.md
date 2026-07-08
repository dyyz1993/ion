# ION CLI 标准用法

> **状态：已验证** — 所有命令均经过 E2E 测试。

## 三大命令

```bash
ion serve start                     # 启动守护进程
ion rpc --method xxx                  # 一问一答（RPC）
ion subscribe --session x             # 实时事件流
```

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
