# ION 测试 Case 完整文档

## 第一部分：单元测试 (Unit Tests)

### 1.1 RPC 协议测试

| # | Case | 输入 | 预期输出 | 验证点 |
|---|------|------|---------|--------|
| U1 | get_state | `{"id":"1","method":"get_state"}` | `{"id":"1","type":"response","command":"get_state","success":true,"data":{...}}` | type=response, success=true, data 有 model/sessionId/messageCount |
| U2 | get_session_stats | `{"id":"2","method":"get_session_stats"}` | data 有 userMessages/assistantMessages/tokens 对象 | camelCase, tokens 是嵌套对象 |
| U3 | get_messages | `{"id":"3","method":"get_messages"}` | data 是消息数组 | 数组结构正确 |
| U4 | get_last_assistant_text | `{"id":"4","method":"get_last_assistant_text"}` | data 是字符串 | 空会话返回 "" |
| U5 | get_tools | `{"id":"5","method":"get_tools"}` | data.tools 是工具数组 | 包在 tools 字段里 |
| U6 | get_active_tools | `{"id":"6","method":"get_active_tools"}` | data 是工具名数组 | 直接数组 |
| U7 | get_available_models | `{"id":"7","method":"get_available_models"}` | data 是模型数组 | 每项有 id/name |
| U8 | get_agents | `{"id":"8","method":"get_agents"}` | data 是 agent 数组 | 每项有 name/description |
| U9 | get_system_prompt | `{"id":"9","method":"get_system_prompt"}` | data 是字符串 | |
| U10 | get_context_usage | `{"id":"10","method":"get_context_usage"}` | data 有 tokens/contextWindow/percent | |
| U11 | 未知命令 | `{"id":"11","method":"nonexistent"}` | `{"success":false,"error":"Unknown command: nonexistent"}` | 格式对齐 pi |
| U12 | ready 信号 | (启动时) | `{"type":"ready",...}` | 第一行输出 |
| U13 | set_model | `{"id":"13","method":"set_model","params":{"provider":"x","modelId":"y"}}` | data 有 modelId/provider | |
| U14 | set_thinking_level | `{"id":"14","method":"set_thinking_level","params":{"level":"high"}}` | data.thinkingLevel="high" | |
| U15 | shutdown | `{"id":"15","method":"shutdown"}` | `{"success":true,"data":null}` 然后退出 | 进程退出 |

### 1.2 全部 75 命令覆盖测试

| # | Case | 验证点 |
|---|------|--------|
| U16 | 75 个命令逐一发送 | 零 Unknown command |

### 1.3 会话存储测试

| # | Case | 验证点 |
|---|------|--------|
| U17 | 创建会话 → 写 JSONL | 文件存在，第一行是 session header |
| U18 | 加载会话 → 恢复消息 | messages 数量正确 |
| U19 | 会话索引实时更新 | sessions.index.json 有记录 |
| U20 | Token 统计准确 | token_input/output 非零 |

### 1.4 插件测试

| # | Case | 验证点 |
|---|------|--------|
| U21 | JSON 扩展加载 | system_prompt 注入成功 |
| U22 | WASM 插件加载 | plugin_version/plugin_init 调用 |
| U23 | WASM 工具注册 | Agent 可见 get_stock_price |
| U24 | Skill 加载 | markdown body 注入 system_prompt |
| U25 | --agent explore | 只读工具，无 edit/write |

---

## 第二部分：集成测试 (Integration Tests)

### 2.1 Manager + Worker 基础

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I1 | Manager 启动 | `ion manager start` | 守护进程运行，0 个 Worker |
| I2 | 创建 Worker | `create_worker({session:"s1"})` | 返回 workerId + sessionId |
| I3 | 列出 Worker | `list_workers()` | 返回 1 个 Worker |
| I4 | 列出项目 | `list_projects()` | 返回当前项目 |
| I5 | 给 Worker 发 prompt | `send_to_session("s1",{"text":"hello"})` | Worker 执行，返回结果 |
| I6 | Worker 状态变化 | Worker busy → idle | 推送 worker_status 事件 |
| I7 | 关闭 Worker | `kill_session("s1")` | Worker 进程退出，推送 worker_destroyed |
| I8 | 关闭后重新启动 | `send_to_session("s1",msg)` | 自动 spawn 新 Worker，加载历史 |

### 2.2 Worker 间通信

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I9 | 同级 A→B 发消息 | A 创建 B (同级) → A send_to_session(B,msg) | B 收到并处理 |
| I10 | 父→子 发消息 | A 创建子 B → A send_to_session(B,msg) | B 收到并处理 |
| I11 | 子→父 回传事件 | A 创建子 B → B 执行 prompt → A 订阅 B | A 收到 child_event |
| I12 | A 拉取 B 状态 | A → get_session(B).get_session_stats() | 返回 B 的统计数据 |
| I13 | A 列子 Worker | A 创建 B,C → A list_workers(parent=A) | 返回 [B,C] |
| I14 | A 停止子 B | A → kill_session(B) | B 退出，A 收到 status_change:dead |
| I15 | B 自己退出 | B shutdown → A 订阅了 B | A 收到 worker_destroyed |

### 2.3 Channel 通信

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I16 | Channel 广播 | A,B 订阅 "review" → C channel_send("review",msg) | A,B 都收到 |
| I17 | Channel 取消订阅 | A 取消订阅 → C channel_send | 只有 B 收到 |
| I18 | 多 Channel | A 订阅 "review"+"deploy" → 两个 channel 各发消息 | A 都收到 |

### 2.4 自动启动

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I19 | 按会话 ID 自动启动 | Worker 不存在 → send_to_session("old-session",msg) | 自动 spawn，加载历史，发消息 |
| I20 | get_session 自动启动 | Worker 不存在 → get_session("old-session") | 自动 spawn，返回 WorkerHandle |

### 2.5 事件推送

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I21 | 订阅 Worker 事件 | subscribe_session("s1") → s1 收到 prompt | 收到 agent_start/text_delta/agent_end |
| I22 | 过滤事件类型 | subscribe("s1",filter=["result"]) | 只收到 result，不收 text_delta |
| I23 | Worker 创建事件 | create_worker | 推送 worker_created (含 sessionId/workerId/project/parent) |
| I24 | 项目变化事件 | Worker 创建/销毁 | 推送 project_changed |
| I25 | 会话变化事件 | Worker 启动 | 推送 session_changed (含 createdBy) |

### 2.6 插件通信

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I26 | 插件 emit 事件 | 插件 api.emit("todo_update",{...}) | 订阅者收到 custom 事件 |
| I27 | 外部调插件方法 | UI → rpc("s1","custom",{"customMethod":"todo_list"}) | 插件处理，返回数据 |
| I28 | 插件创建子 Worker | 插件 api.create_worker() | Manager spawn，返回 workerId |

### 2.7 UI 对接

| # | Case | 步骤 | 验证点 |
|---|------|------|--------|
| I29 | 全局概览 | get_overview() | 返回所有 Worker + 项目 + 会话 |
| I30 | 多 Worker 同时订阅 | subscribe("s1") + subscribe("s2") | 两个 Worker 事件都收到，worker_id 区分 |
| I31 | 历史加载 | rpc("s1","get_messages") | 返回完整消息列表 |
| I32 | 导出会话 | rpc("s1","export_html") | 返回 HTML 文件路径 |

---

## 第三部分：端到端场景测试 (E2E Scenarios)

### 场景 E1：代码审查流水线

```
前置: Manager 已启动

Step 1: 创建协调者 Worker
  → create_worker({session:"coordinator", agent:"plan"})
  → 返回 wkr_coord

Step 2: 协调者分析项目，创建审查子 Worker
  → wkr_coord 执行 prompt("分析项目结构，为每个模块创建审查子任务")
  → 插件自动创建:
     create_worker({session:"review-auth", parent:"coordinator", channel:"review"})
     create_worker({session:"review-api", parent:"coordinator", channel:"review"})

Step 3: 协调者订阅子 Worker 事件
  → subscribe_session("review-auth")
  → subscribe_session("review-api")

Step 4: 子 Worker 执行审查
  → send_to_session("review-auth", "审查 auth 模块")
  → send_to_session("review-api", "审查 api 模块")

Step 5: 协调者实时观察
  → 收到 child_event (review-auth: text_delta)
  → 收到 child_event (review-api: tool_call: read)
  → 收到 child_event (review-auth: result)

Step 6: 协调者汇总
  → 两个子 Worker 都完成后
  → 协调者输出汇总报告

验证:
  ✅ 2 个子 Worker 被创建
  ✅ 协调者收到子 Worker 的事件
  ✅ 子 Worker 状态正确变化 (idle→busy→idle)
  ✅ 最终汇总报告包含两个模块的审查结果
```

### 场景 E2：多项目并发

```
Step 1: 创建两个项目的 Worker
  → create_worker({session:"proj-a-task", project:"~/proj-a"})
  → create_worker({session:"proj-b-task", project:"~/proj-b"})

Step 2: 同时执行
  → send_to_session("proj-a-task", "分析代码")
  → send_to_session("proj-b-task", "分析代码")

Step 3: 验证隔离
  → proj-a 的 Worker 不知道 proj-b 的存在
  → list_projects() 返回两个项目

验证:
  ✅ 两个 Worker 互不影响
  ✅ 项目隔离正确
  ✅ list_projects 返回 2 个项目
```

### 场景 E3：Channel 协作

```
Step 1: 创建 3 个 Worker 都订阅 "deploy" channel
  → create_worker({session:"builder", channel:"deploy"})
  → create_worker({session:"tester", channel:"deploy"})
  → create_worker({session:"deployer", channel:"deploy"})

Step 2: builder 完成后通知
  → builder 的插件: channel_send("deploy", {type:"build_complete"})

Step 3: tester 收到通知，开始测试
  → tester 收到 channel_msg
  → tester 执行测试

Step 4: tester 完成后通知
  → channel_send("deploy", {type:"test_passed"})

Step 5: deployer 收到通知，执行部署
  → deployer 收到 channel_msg
  → deployer 执行部署

验证:
  ✅ channel 消息按顺序传递
  ✅ 三个 Worker 都收到各自需要的消息
  ✅ 链路: build → test → deploy
```

### 场景 E4：会话恢复

```
Step 1: Worker 运行并产生历史
  → create_worker({session:"recover-test"})
  → send_to_session("recover-test", "记住我的名字是 Alice")
  → Worker 回复

Step 2: 关闭 Worker
  → kill_session("recover-test")

Step 3: 重新启动同一个会话
  → send_to_session("recover-test", "我叫什么名字？")
  → Manager 自动 spawn 新 Worker
  → Worker 加载 JSONL 历史
  → Worker 回复 "Alice"

验证:
  ✅ 会话历史持久化
  ✅ 自动启动正确恢复上下文
  ✅ Worker 知道之前的对话
```

### 场景 E5：UI 实时监控

```
Step 1: UI 连接 Manager (WebSocket)
  → ws_connect

Step 2: 订阅全局概览
  → subscribe_overview()
  → 收到所有 Worker 状态

Step 3: 创建 Worker
  → 收到 worker_created 事件

Step 4: 订阅特定 Worker 的流式输出
  → subscribe_session("s1", filter=["text_delta","tool_call","result"])
  → 实时收到逐 token 输出

Step 5: 切换到另一个 Worker
  → unsubscribe("s1")
  → subscribe_session("s2")
  → 只收 s2 的事件

验证:
  ✅ 概览正确反映所有 Worker
  ✅ 流式事件实时到达
  ✅ 切换订阅不丢数据
```

---

## 第四部分：压力测试

| # | Case | 验证点 |
|---|------|--------|
| S1 | 10 个 Worker 同时 prompt | 全部完成，无死锁 |
| S2 | 1 个 Worker 连续 50 轮对话 | 无内存泄漏 |
| S3 | Channel 100 条消息广播 | 全部订阅者收到 |
| S4 | 快速创建/销毁 20 个 Worker | 无僵尸进程 |
| S5 | Manager 重启后恢复 | Worker 从 JSONL 恢复历史 |

---

## 执行顺序

```
Phase 1: 单元测试 U1-U25 (RPC + 存储 + 插件)
Phase 2: 集成测试 I1-I8 (Manager 基础)
Phase 3: 集成测试 I9-I20 (Worker 通信 + 自动启动)
Phase 4: 集成测试 I21-I32 (事件 + UI)
Phase 5: E2E E1-E5 (完整场景)
Phase 6: 压力测试 S1-S5
```
