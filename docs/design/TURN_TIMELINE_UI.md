# Turn Timeline UI — 前端接口契约

> **状态：已完成** — 接口已实现，前端可直接对接。真实 LLM 验证通过（durationMs + assistantContent）。

本文档定义"对话时间线"UI（用户输入 → 折叠的工作过程 → 最终回复）所需的全部接口、数据裁剪策略、以及多用户同屏场景下的事件流。

---

## 1. UI 元素与数据来源

```
帮我分析一下这个项目的架构          ← ① 用户输入（userContent）
▸ 工作了2分7秒 · 6步               ← ② 折叠条（durationMs + toolCallCount）
这个项目采用分层架构：内核层…        ← ③ 最终回复（assistantContent）
```

展开折叠条后：

```
▾ 工作了2分7秒 · 6步
  💭 用户要分析项目架构…             ← thinking
  📁 ls   {"path":"src/"} → agent/… ← 工具调用（name + args + result）
  📖 read {"file_path":"Cargo.toml"} → name=ion…
  🔍 grep → 128 matches
```

| UI 元素 | 数据来源 | 何时获取 |
|---------|---------|---------|
| ① 用户输入 | `list_turns.userContent` | 列表加载时 |
| ② 折叠条 | `list_turns.durationMs` + `toolCallCount` | 列表加载时 |
| ③ 最终回复 | `list_turns.assistantContent` | 列表加载时 |
| 展开后的步骤 | `get_turn_detail.entries[]` | **用户点击展开时（懒加载）** |

---

## 2. 数据裁剪策略（关键：支持长历史）

### 2.1 `list_turns` 默认是轻量裁剪版

```
请求：ion rpc --session <sid> --method list_turns
      （默认 full_content=false, limit=50）
```

| 字段 | 默认行为 | 体积 |
|------|---------|------|
| `userContent` | **截断到 200 字** | 小 |
| `assistantContent` | **截断到 200 字** | 小 |
| `keySteps` | 只有工具名 `["read","write"]`，**不含参数和结果** | 小 |
| `toolCallCount` | 数字 | 小 |
| `durationMs` | 完整毫秒 | 小 |
| `tokens` | `{input, output}` | 小 |
| 步骤详情（参数/结果/thinking 文本） | **完全不返回** | — |

**单条 turn 约 200–500 字节**。加载 1000 条历史 ≈ 几百 KB，秒级返回。

### 2.2 分页（长历史滚动加载）

```
首次：  list_turns  (limit=50)            → hasMore=true, nextCursor="..."
下翻：  list_turns  (limit=50, after=<nextCursor>)
```

| 参数 | 默认 | 说明 |
|------|------|------|
| `limit` | 50 | 每页条数 |
| `after` | 无 | 游标（取上一次返回的 `nextCursor`） |
| `full_content` | false | true 时不截断 content（更精确但更大） |

返回含 `hasMore` / `totalCount` / `nextCursor`。

### 2.3 展开详情（懒加载，不随列表返回）

折叠条里的步骤详情**不在 `list_turns` 里**，用户点击展开时单独调：

```
ion rpc --session <sid> --method get_turn_detail --params '{"turnId":"3"}'
```

返回 `entries[]`，每个 entry 是 LLM 的一步输出：

| 步骤类型 | 结构 |
|---------|------|
| thinking | `{message:{Assistant:{content:[{Thinking:{thinking:"..."}}]}}}` |
| text（中间文本） | `{message:{Assistant:{content:[{Text:{text:"..."}}]}}}` |
| 工具调用 | `{message:{Assistant:{content:[{ToolCall:{name,args,id}}]}}}` |
| 工具结果 | `{message:{ToolResult:{content:[{Text:{text:"..."}}]}}}` |

前端解析这些 entry 渲染成步骤列表（解析逻辑见本文档 §5）。

> **为什么这样设计**：工具调用的参数和结果可能很大（read 一个文件几千行），如果随 `list_turns` 返回会让历史列表臃肿。懒加载让"列表浏览"和"详情查看"分离，互不影响。

---

## 3. 接口清单

### 3.1 Pull（主动查询）

| # | 接口 | 何时调 | 关键返回字段 |
|---|------|--------|------------|
| 1 | `subscribe --session <sid>` | 页面加载时（建推送通道） | 后续所有实时事件 |
| 2 | `list_turns` | 页面加载 / agent_end 后 / F5 刷新 | turns[]（轻量裁剪版） |
| 3 | `get_turn_detail` | 用户展开某 turn | entries[]（步骤详情） |
| 4 | `prompt` | 用户发消息 | 异步触发，立即返回 null |
| 5 | `get_context_usage` | 可选，显示 token 用量条 | messageCount, usagePercent, contextWindow |
| 6 | `get_last_assistant_text` | 可选，只拿最后一条回复（轻量） | 纯字符串 |

### 3.2 Push（host 主动推送，所有 subscriber 都收到）

通过 `subscribe --session <sid>` 建立通道后，以下事件**自动推送**，前端无需轮询：

| 事件 | 干什么 | 关键字段 |
|------|--------|---------|
| `agent_start` | 显示折叠条"工作中..." | `sessionId, timestamp` |
| `text_delta` | （可选）实时字符累加 | `delta` |
| `tool_execution_start` | 工具开始（展开态显示） | `toolCallId, toolName, args` |
| `tool_execution_end` | 工具完成，耗时回填 | `toolCallId, durationMs, isError` |
| `agent_end` | 折叠条更新"工作了X秒" + 触发 list_turns 补全 | `sessionId, timestamp` |
| `error` | 显示错误态 | `message, timestamp` |
| `child_crashed` | worker 崩溃 | `worker_id, exit_code, exit_reason` |
| `ApprovalRequest` | 文件审批弹窗（多端协同） | `requestId, files[]` |
| `ApprovalResolved` | 审批结果同步 | `requestId, response` |

> **Instance 订阅（`--session`）直接收 worker 原始事件**，不经 EventBus 过滤。一次订阅拿到该 session 的全部事件。

---

## 4. 多用户同屏场景（手机 / 桌面 / 电脑）

设定：3 个终端同时打开同一个 session 页面，host 远端 `ion serve` 运行中。

### 4.1 实时流刷新（数据怎么加载）

```
用户在屏幕 A 发 prompt
    │
    ▼
host 收到 prompt → 启动 agent
    │
    ▼
agent_start 事件 ──broadcast──► 3 个屏幕的订阅都收到
    │                              ├─ A: 显示折叠条"工作中..."
    │                              ├─ B: 同步显示
    │                              └─ C: 同步显示
    ▼
text_delta × N ──broadcast──► 3 个屏幕（可选实时累加字符）
    ▼
tool_execution_start/end ──broadcast──► 工具步骤实时显示
    ▼
agent_end 事件 ──broadcast──► 3 个屏幕都收到
    │                              ├─ 折叠条更新"工作了3.5秒 · 2步"
    │                              └─ 触发 list_turns 补全 content（每个屏幕各自调一次）
```

**关键**：实时事件是 host **主动 push**，3 个屏幕同时收到，**不需要各自轮询**。

### 4.2 进程关闭后重新载入（冷启动）

用户关了浏览器，第二天重新打开：

```
打开页面
    │
    ▼
list_turns ──Pull──► 从磁盘 jsonl 读，拿到全部历史 turn
    │                  （含 durationMs + 截断的 assistantContent）
    ▼
渲染：用户输入 + 折叠条（有 durationMs）+ 最终回复（有 assistantContent）
    │
    ▼
（用户点击展开某个 turn）
    │
    ▼
get_turn_detail ──Pull──► 拿到该 turn 的 entries（步骤详情）
```

**关键**：冷启动**只调一次 `list_turns`**，不要预加载所有 `get_turn_detail`。展开时才懒加载。

### 4.3 实时过程中刷新（F5）

agent 正在跑，用户按 F5：

```
F5 刷新
    │
    ▼
list_turns ──Pull──► 拿到已完成的 turn（正在进行的可能还没落盘）
    │
    ▼
subscribe --session <sid> ──重建订阅──► 继续收后续事件
    │
    ▼
（agent_end 来时，再调一次 list_turns 补全最新 turn）
```

**关键**：刷新后**两步缺一不可**：
- 只 Pull 不订阅 → 漏后续事件
- 只订阅不 Pull → 缺已有历史

### 4.4 实时过程中，大家看到的是什么？

| 时刻 | 屏幕 A（发 prompt） | 屏幕 B（旁观） | 屏幕 C（刚 F5） |
|------|---------------------|-------------|-------------------|
| t0 发 prompt | "我：帮我分析架构" + 折叠条"工作中..." | 收到 agent_start，同步显示 | （还没打开） |
| t1 第一个工具 | 折叠条"1步" | 同步收到"1步" | 打开 → list_turns 拉已有 + subscribe 重建 → 显示"工作中..." |
| t2 agent_end | "工作了3.5秒 · 2步" + 最终回复 | 同步更新 | 同步更新 |
| t3 新 prompt | 循环 | 同步 | 同步 |

**3 个屏幕状态完全一致**——因为：
1. 实时事件走 **Push**（host broadcast 给所有 subscriber）
2. 历史数据走 **Pull**（`list_turns` 读磁盘快照，所有屏幕读到的一致）
3. 订阅通道**每个客户端独立建立**（独立 socket 连接），互不影响

---

## 5. entry 解析参考（get_turn_detail 返回的 entries）

```javascript
// entries 是 JSONL 行数组，每行一个 entry
// 解析 message entry 提取步骤
function extractSteps(entries) {
  const steps = [];
  for (const entry of entries) {
    if (entry.type !== 'message') continue;
    const msg = entry.message || {};

    // Assistant 消息（content 数组，含 Text/ToolCall/Thinking）
    if (msg.Assistant) {
      for (const block of msg.Assistant.content || []) {
        if (block.ToolCall) {
          steps.push({
            type: 'tool_call',
            name: block.ToolCall.name,
            args: block.ToolCall.arguments,
            id: block.ToolCall.id
          });
        } else if (block.Thinking) {
          steps.push({ type: 'thinking', text: block.Thinking.thinking });
        } else if (block.Text) {
          steps.push({ type: 'text', text: block.Text.text });
        }
      }
    }

    // ToolResult（关联到上一个 tool_call）
    if (msg.ToolResult) {
      const content = msg.ToolResult.content || [];
      const text = (Array.isArray(content) ? content : [content])
        .map(b => b.Text?.text || (typeof b === 'string' ? b : JSON.stringify(b)))
        .join('');
      // 回填到最近的 tool_call
      for (let i = steps.length - 1; i >= 0; i--) {
        if (steps[i].type === 'tool_call' && !steps[i].result) {
          steps[i].result = text;
          break;
        }
      }
    }
  }
  return steps;
}
```

---

## 6. 设计原则

> **Push 负责"状态变化的实时通知"，Pull 负责"完整数据的按需获取"。两者必须同时存在。**

| 只有 Push | 只有 Pull | Push + Pull（正确） |
|-----------|-----------|-------------------|
| 新连接看不到历史（空白） | 多屏幕不同步，必须手动刷新 | ✅ 历史完整 + 实时同步 |
| 刷新后丢失进行中状态 | 实时性差 | ✅ 冷启动 + 实时都覆盖 |

### 长历史支持的核心设计

- **列表（list_turns）轻量裁剪**：content 截断 200 字，不含步骤详情 → 单条几百字节
- **分页（limit + nextCursor）**：滚动加载，不一次拉全量
- **详情（get_turn_detail）懒加载**：展开时才调，避免预加载浪费

这样即使有 10000 条历史对话，列表加载也很快（分页 + 裁剪），只有用户真正展开的 turn 才会触发详情查询。
