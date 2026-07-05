# Session 消息系统（对齐 pi）

> 本文档描述 ION 会话消息的 Entry 类型、推送通道、消费方归类。
> 插件开发者和内核贡献者应以此为准，判断某条消息该走哪个通道。

## 一、三大"显式"发送方式

这三种走完整 agent loop，最终都是 `role: "user"` 的消息被 LLM 看到：

| 方式 | API | role | 消费时机 | 备注 |
|------|-----|------|---------|------|
| **prompt** | `session.prompt()` | `user` | 空闲时立即；流式时需指定 behavior | 最标准的用户消息 |
| **steer** | `session.steer()` | `user` | 入 steering 队列，下轮 turn 开始前注入 | 高优先级，可带 `immediate` 打断 |
| **followUp** | `session.followUp()` | `user` | 入 follow-up 队列，内循环结束后注入 | 低优先级 |

**特点：** 都会走完整流程（扩展事件→模板展开→模型校验→agent loop→持久化到 jsonl）。全部 **LLM 可见 + UI 可见**。

## 二、直接往 jsonl 插数据的 Entry 类型

由扩展或内部逻辑直接调用 session 的 append 方法，不走完整 prompt 流程。

### （A）LLM 可见 + UI 有特殊呈现

| 方法 | Entry type | role（给 LLM ） | UI 呈现 | 用途举例 |
|------|-----------|----------------|---------|---------|
| `appendMessage(msg)` 且 `role=bashExecution` | `SessionMessageEntry` | `bashExecution` → 转 `user` | bash 执行卡片 | 用户 `!ls` 的结果 |
| `appendCompaction(summary)` | `CompactionEntry` | `compactionSummary` → 转 `user` | 折叠摘要卡片 | compaction 后 |
| `appendBranchSummary(fromId, summary)` | `BranchSummaryEntry` | `branchSummary` → 转 `user` | 分支摘要卡片 | 树形导航时 |
| `appendCustomMessageEntry(type, content, display=true)` | `CustomMessageEntry` | `custom` → 转 `user` | 按 customType 渲染 | 扩展注入 |
| `appendSystemEvent(type, label, data, display=true)` | `SystemEventEntry` | `custom` → 转 `user` | 系统事件条 | 模型/agent 切换 |

**LLM 看到的转换：**

```
bashExecution → user: `command`\n```\noutput\n```
compactionSummary → user: <summary>...</summary>
branchSummary → user: <summary>...</summary>
custom → user: 原样 content
```

### （B）LLM 可见 + UI 不可见（纯上下文注入）

| 方法 | Entry type | 用途 |
|------|-----------|------|
| `appendSystemEvent(type, label, data, display=false)` | `SystemEventEntry` | 系统变更通知，LLM 需要知道但 UI 不展示 |
| `appendCustomMessageEntry(type, content, display=false)` | `CustomMessageEntry` | 扩展想喂给 LLM 但不想污染 UI |

### （C）LLM 完全不可见（纯记录/状态持久化）

| 方法 | Entry type | 用途 |
|------|-----------|------|
| `appendCustomEntry(type, data)` | `CustomEntry` | 扩展状态持久化，LLM 和 UI 都看不到，仅存在 jsonl |
| `appendThinkingLevelChange(level)` | `ThinkingLevelChangeEntry` | 记录 thinking level 变更史 |
| `appendModelChange(provider, modelId)` | `ModelChangeEntry` | 记录模型变更史 |
| `appendAgentChange(name, config)` | `AgentChangeEntry` | 记录 agent 切换史 |
| `appendSessionName(name)` | `SessionInfoEntry` | 记录 session 名称 |
| `appendLabel(targetId, label)` | `LabelEntry` | 书签标记 |

## 三、sendCustomMessage 的三种投递模式

扩展通过 `context.sendMessage()` / `session.sendCustomMessage()` 注入时，有 `deliverAs` 选项：

| deliverAs | 流式中 | 空闲时 + triggerTurn | 空闲时 + 无 trigger |
|-----------|--------|---------------------|-------------------|
| `"steer"` | 入 steering 队列 | 触发新 turn | — |
| `"followUp"` | 入 follow-up 队列 | 触发新 turn | — |
| `"nextTurn"` | 入 `_pendingNextTurnMessages`，等下次 prompt 注入 | 同上 | — |

## 四、选择决策树

```
要发给 LLM？
  ├── 要走完整 prompt 流程（模板展开/模型校验）？
  │     ├── 是 → prompt / steer / followUp
  │     └── 否：
  │           ├── 是 bash 执行结果？ → appendMessage(role=bashExecution)
  │           ├── 是 compaction 摘要？ → appendCompaction()
  │           ├── 是分支摘要？ → appendBranchSummary()
  │           ├── 是扩展自定义消息？ → appendCustomMessageEntry()
  │           └── 是系统事件（模型切换等）？ → appendSystemEvent()
  │
  ├── UI 要特殊渲染？
  │     ├── 是 → display=true
  │     └── 否 → display=false（LLM 可见，UI 不可见）
  │
  └── 不发给 LLM？
        ├── 是扩展状态持久化？ → appendCustomEntry()
        ├── 是模型/thinkingLevel/agent 变更记录？ → appendModelChange/appendThinkingLevelChange/appendAgentChange()
        └── 是 session 名称/书签？ → appendSessionName() / appendLabel()
```

## 五、对照：现有 ION Entry 类型

| ION type | 对应 pi Entry | 当前状态 |
|----------|-------------|---------|
| `session` | SessionHeader | ✅ 有 |
| `message` | SessionMessageEntry | ✅ 有（User/Assistant/Tool） |
| `model_change` | ModelChangeEntry | ✅ 有 |
| `thinking_level_change` | ThinkingLevelChangeEntry | ✅ 有 |
| `agent_change` | AgentChangeEntry | ✅ 有 |
| `session_info` | SessionInfoEntry | ✅ 有 |
| `compaction` | CompactionEntry | ✅ 有 |
| `branch_summary` | BranchSummaryEntry | ✅ 有 |
| `custom` | CustomEntry | ✅ 有 |
| `bash_execution` | 无独立 type（走 message+bashExecution role） | ✅ 有 Message::BashExecution 变体 |
| `custom_message` | CustomMessageEntry | ✅ 有 |
| `system_event` | SystemEventEntry | ✅ 有 |
| `label` | LabelEntry | ✅ 有 |
| `active_tools_change` | ActiveToolsChangeEntry | ✅ 有（新增） |

## 六、对齐 pi 需要的改动

| 改动 | 原因 | 状态 |
|------|------|------|
| `message.role` 加 `bashExecution`、`custom`、`compactionSummary`、`branchSummary` | 对齐 pi messages.ts | ✅ 已完成（4 个新 Message 变体） |
| 新增 `CustomMessageEntry`（带 `display` + `details` 字段） | 扩展自定义消息 | ✅ 已完成（`append_custom_message` RPC） |
| 新增 `SystemEventEntry`（带 `display` + `label` 字段） | 系统事件通知 | ✅ 已完成（`append_system_event` RPC） |
| 新增 `LabelEntry` | 书签 | ✅ 已完成（`append_label` RPC） |
| `appendCustomMessageEntry()` / `appendSystemEvent()` / `appendLabel()` | 对应的 append 方法 | ✅ 已完成 |
| `sendCustomMessage(deliverAs)` | 扩展投递消息的入口 | ✅ 已完成 |

## 七、CLI 测试清单

实现后，每个 RPC 都可以通过 `ion rpc --session x` 直接验证。

### （A）消息类（LLM 可见 + UI 可见/特殊渲染）

| RPC | 测试命令 | 预期结果 |
|-----|---------|---------|
| `prompt` | `ion rpc --session x --method prompt --params '{"text":"hello"}'` | 消息出现在 get_messages 中 |
| `steer` | `ion rpc --session x --method steer --params '{"text":"steer msg"}'` | steering 队列 + 下轮注入 |
| `follow_up` | `ion rpc --session x --method follow_up --params '{"text":"follow msg"}'` | follow_up 队列 + 内循环结束注入 |
| `append_custom_message` | `ion rpc --session x --method append_custom_message --params '{"type":"bash_result","content":"<bash_result>✅ done</bash_result>","display":true}'` | `get_messages` 中多一条 custom role 消息，UI 按 type 渲染 |
| `append_system_event` (display=true) | `ion rpc --session x --method append_system_event --params '{"type":"model_change","label":"切换模型","display":true}'` | `get_messages` 中多一条，UI 显示系统事件条 |
| `append_system_event` (display=false) | `ion rpc --session x --method append_system_event --params '{"type":"internal","label":"后台状态变更","display":false}'` | `get_messages` 中多一条，UI 不可见 |

### （B）纯持久化类（LLM 不可见）

| RPC | 测试命令 | 预期结果 |
|-----|---------|---------|
| `append_custom_entry` | `ion rpc --session x --method append_custom_entry --params '{"type":"file_snapshot","data":{"path":"a.rs","hash":"abc"}}'` | session.jsonl 中多一条 custom entry，`get_messages` 不显示 |
| `append_model_change` | `ion rpc --session x --method append_model_change --params '{"provider":"zhipuai","modelId":"glm-4.7"}'` | session.jsonl 中多一条 model_change |
| `append_thinking_level_change` | `ion rpc --session x --method append_thinking_level_change --params '{"level":"off"}'` | session.jsonl 中多一条 thinking_level_change |
| `append_agent_change` | `ion rpc --session x --method append_agent_change --params '{"name":"coordinator"}'` | session.jsonl 中多一条 agent_change |
| `append_session_name` | `ion rpc --session x --method append_session_name --params '{"name":"我的会话"}'` | session.jsonl 中 session_info 更新 |
| `append_label` | `ion rpc --session x --method append_label --params '{"targetId":"msg_xxx","label":"重要节点"}'` | session.jsonl 中多一条 label entry |

### （C）扩展投递类

| RPC | 测试命令 | 预期结果 |
|-----|---------|---------|
| `send_custom_message` (deliverAs=followUp) | `ion rpc --session x --method send_custom_message --params '{"type":"bash_result","content":"<bash_result>✅ done</bash_result>","deliverAs":"followUp"}'` | 进入 follow_up 队列，当前 agent 停止后注入 |
| `send_custom_message` (deliverAs=steer) | `ion rpc --session x --method send_custom_message --params '{"type":"alert","content":"立即处理","deliverAs":"steer"}'` | 进入 steering 队列，下轮 turn 前注入 |
| `send_custom_message` (deliverAs=nextTurn) | `ion rpc --session x --method send_custom_message --params '{"type":"note","content":"稍后处理","deliverAs":"nextTurn"}'` | 入 `_pendingNextTurnMessages`，下次 prompt 时注入 |

### （D）验证命令

```bash
# 读所有消息
ion rpc --session x --method get_messages

# 直接查 session.jsonl
cat ~/.ion/agent/sessions/--hash--name--/session.jsonl

# 订阅实时事件
ion subscribe --session x
```

### （E）当前实现状态

| RPC | ion_worker.rs | 状态 |
|-----|--------------|------|
| `get_messages` | `"get_messages"` | ✅ 已实现 |
| `append_system_event` | `"append_system_event"` | ✅ 已实现 |
| `append_custom_message` | `"append_custom_message"` | ✅ 已实现 |
| `append_custom_entry` | `"append_custom_entry"` | ✅ 已实现 |
| `send_custom_message` | `"send_custom_message"` | ✅ 已实现 |
| `append_model_change` | `"append_model_change"` | ✅ 已实现 |
| `append_thinking_level_change` | `"append_thinking_level_change"` | ✅ 已实现 |
| `append_agent_change` | `"append_agent_change"` | ✅ 已实现 |
| `append_session_name` | `"append_session_name"` | ✅ 已实现 |
| `append_label` | `"append_label"` | ✅ 已实现 |
| `append_active_tools_change` | `"append_active_tools_change"` | ✅ 已实现 |

## 八、核查清单（CLI + 数据获取 + JSONL 结构）

全部 14 个 case 按三大类汇总。每个 case 列出 CLI 命令、如何获取数据验证、预期 JSONL 结构。

### （A）消息类 — LLM 可见 + UI 可见/特殊渲染

| # | Case | CLI 命令 | 数据获取 | 预期 JSONL 结构 |
|---|------|---------|---------|----------------|
| A1 | `prompt` 发消息 | `ion rpc --session x --method prompt --params '{"text":"hello"}'` | `ion rpc --session x --method get_messages` | `{"type":"message","message":{"role":"user",...}}` |
| A2 | `steer` 高优注入 | `ion rpc --session x --method steer --params '{"text":"steer msg"}'` | 同 session get_messages，下轮 turn 前出现 | 同 A1 |
| A3 | `follow_up` 低优注入 | `ion rpc --session x --method follow_up --params '{"text":"follow msg"}'` | 同 session get_messages，内循环结束后出现 | 同 A1 |
| A4 | `append_custom_message` | `ion rpc --session x --method append_custom_message --params '{"type":"bash_result","content":"<result>ok</result>","display":true}'` | `cat ~/.ion/agent/sessions/--*--/session.jsonl \| grep custom_message` | `{"type":"custom_message","customType":"bash_result","content":"<result>ok</result>","display":true}` |
| A5 | `append_system_event`(display=true) | `ion rpc --session x --method append_system_event --params '{"type":"model_change","label":"切换","display":true}'` | `cat session.jsonl \| grep system_event` | `{"type":"system_event","customType":"model_change","label":"切换","display":true}` |
| A6 | `append_system_event`(display=false) | `ion rpc --session x --method append_system_event --params '{"type":"internal","label":"后台","display":false}'` | 同上 | 同上，`display:false` |

### （B）纯持久化类 — LLM 不可见

| # | Case | CLI 命令 | 数据获取 | 预期 JSONL 结构 |
|---|------|---------|---------|----------------|
| B1 | `append_custom_entry` | `ion rpc --session x --method append_custom_entry --params '{"type":"file_snapshot","data":{"path":"a.rs","hash":"abc"}}'` | `cat session.jsonl \| grep "type":"custom"` | `{"type":"custom","customType":"file_snapshot","data":{...}}` |
| B2 | `append_model_change` | `ion rpc --session x --method append_model_change --params '{"provider":"zhipuai","modelId":"glm-4.7"}'` | `cat session.jsonl \| grep model_change` | `{"type":"model_change","provider":"zhipuai","modelId":"glm-4.7"}` |
| B3 | `append_thinking_level_change` | `ion rpc --session x --method append_thinking_level_change --params '{"level":"off"}'` | `cat session.jsonl \| grep thinking_level_change` | `{"type":"thinking_level_change","level":"off"}` |
| B4 | `append_agent_change` | `ion rpc --session x --method append_agent_change --params '{"name":"coordinator"}'` | `cat session.jsonl \| grep agent_change` | `{"type":"agent_change","name":"coordinator"}` |
| B5 | `append_session_name` | `ion rpc --session x --method append_session_name --params '{"name":"我的会话"}'` | `cat session.jsonl \| grep session_info` | `{"type":"session_info","name":"我的会话"}` |
| B6 | `append_label` | `ion rpc --session x --method append_label --params '{"targetId":"msg_x","label":"重要"}'` | `cat session.jsonl \| grep "type":"label"` | `{"type":"label","targetId":"msg_x","label":"重要"}` |
| B7 | `append_active_tools_change` | `ion rpc --session x --method append_active_tools_change --params '{"activeToolNames":["bash","read"]}'` | `cat session.jsonl \| grep active_tools_change` | `{"type":"active_tools_change","activeToolNames":["bash","read"]}` |

### （C）扩展投递类 — 通过 agent follow_up/steer 队列注入

| # | Case | CLI 命令 | 数据获取 | 预期行为 |
|---|------|---------|---------|---------|
| C1 | `send_custom_message`(followUp) | `ion rpc --session x --method send_custom_message --params '{"type":"note","content":"hi","deliverAs":"followUp"}'` | 执行 prompt 后 g`et_messages` 中出现该消息 | 入 follow_up 队列，agent 空闲后注入 |
| C2 | `send_custom_message`(steer) | `ion rpc --session x --method send_custom_message --params '{"type":"alert","content":"urgent","deliverAs":"steer"}'` | 同 C1 | 入 steering 队列，下轮 turn 前注入 |
| C3 | `send_custom_message`(nextTurn) | `ion rpc --session x --method send_custom_message --params '{"type":"note","content":"later","deliverAs":"nextTurn"}'` | 同 C1 | 入 pending 队列，下次 prompt 时注入 |

### （D）验证辅助命令

```bash
# 查看会话所有消息（LLM 可见的完整消息列表）
ion rpc --session <sid> --method get_messages

# 查看 session.jsonl 原始数据（所有 entry 类型）
cat ~/.ion/agent/sessions/--*/session.jsonl

# 只看某种 entry 类型
grep '"type":"custom_message"' ~/.ion/agent/sessions/--*/session.jsonl

# 订阅实时事件（包括 append 操作的通知）
ion subscribe --session <sid>

# CI 一键执行全部 14 个 case
bash tests/session_entries_ci.sh
```

### （E）字段平铺验证（修复后的格式要求）

以下 entry 类型的字段必须平铺在 JSON 顶层，不能嵌套在 `data` 里：

| Entry 类型 | 必须平铺的字段 | 验证命令 |
|-----------|--------------|---------|
| `custom_message` | `customType`, `content`, `display`, `details` | `grep custom_message session.jsonl \| python3 -c "import sys,json; e=json.loads(sys.stdin.read()); assert 'data' not in e"` |
| `system_event` | `customType`, `label`, `display` | `grep system_event session.jsonl \| python3 -c ...` |
| `label` | `targetId`, `label` | `grep "type":"label" session.jsonl \| python3 -c ...` |
| `active_tools_change` | `activeToolNames` | `grep active_tools_change session.jsonl \| python3 -c ...` |


