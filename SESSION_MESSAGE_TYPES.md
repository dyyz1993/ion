# Session 消息类型扩展（对齐 pi AgentMessage + SessionTreeEntry）

> **状态：已验证** — 4 个新 Message 变体 + 1 个新 SessionEntry + `append_session_entry` 字段平铺 bug 修复全部完成。90 个 lib 测试通过，真实 LLM 调用验证 provider 不 panic。

## 一、目标

把 ION 的 Message enum 从 3 变体扩到 7 变体，对齐 pi 的 `AgentMessage` 联合类型；同时补齐 `active_tools_change` 这一项 SessionEntry，对齐 pi 的 `SessionTreeEntry` 联合。

完成后，ION 与 pi 在「对话消息」和「会话条目」两个维度完全对齐，迁移任何 pi 上层功能（bash 卡片渲染、压缩摘要展示、工具切换记录等）都不再需要补类型。

## 二、pi 参考

| pi 类型 | pi 文件 | 用途 |
|---------|---------|------|
| `BashExecutionMessage` | `packages/agent/src/harness/messages.ts:19` | `!ls` 这类用户直发 bash 的执行结果 |
| `CustomMessage<T>` | `packages/agent/src/harness/messages.ts:31` | 扩展自定义消息（content + display + details） |
| `BranchSummaryMessage` | `packages/agent/src/harness/messages.ts:40` | 分支回到主线时的摘要 |
| `CompactionSummaryMessage` | `packages/agent/src/harness/messages.ts:47` | 压缩后的摘要 |
| `ActiveToolsChangeEntry` | `packages/agent/src/harness/types.ts:357` | 工具集变更记录（独立 entry type） |
| `convertToLlm()` | `packages/agent/src/harness/messages.ts:120` | 自定义 role → user 的转换中心 |
| `SessionTreeEntry` 联合 | `packages/agent/src/harness/types.ts:409` | 全部 11 种 entry 类型 |

## 三、ION 现状 vs 目标

### Message enum（对话消息）

| 变体 | ION 现状 | pi 对应 | 改动 |
|------|---------|---------|------|
| `User` | ✅ | `UserMessage` | 无 |
| `Assistant` | ✅ | `AssistantMessage` | 无 |
| `ToolResult` | ✅ | `ToolResultMessage` | 无 |
| `BashExecution` | ❌ | `BashExecutionMessage` | **新增** |
| `Custom` | ❌ | `CustomMessage` | **新增** |
| `BranchSummary` | ❌ | `BranchSummaryMessage` | **新增** |
| `CompactionSummary` | ❌ | `CompactionSummaryMessage` | **新增** |

### SessionEntry（会话条目）

| Entry type | ION 现状 | pi 对应 | 改动 |
|-----------|---------|---------|------|
| `message` | ✅ struct | `MessageEntry` | 无 |
| `model_change` | ✅ struct | `ModelChangeEntry` | 无 |
| `thinking_level_change` | ✅ struct | `ThinkingLevelChangeEntry` | 无 |
| `agent_change` | ✅ struct | `AgentChangeEntry` | 无 |
| `session_info` | ✅ struct | `SessionInfoEntry` | 无 |
| `compaction` | ✅ struct | `CompactionEntry` | 无 |
| `branch_summary` | ✅ struct | `BranchSummaryEntry` | 无 |
| `custom` | ✅ struct | `CustomEntry` | 无 |
| `custom_message` | ⚠️ 仅 RPC | `CustomMessageEntry` | **加 struct** |
| `system_event` | ⚠️ 仅 RPC | `SystemEventEntry`（ION 原创） | **加 struct** |
| `label` | ⚠️ 仅 RPC | `LabelEntry` | **加 struct** |
| `active_tools_change` | ❌ | `ActiveToolsChangeEntry` | **加 struct + RPC** |

> 注：`custom_message`/`system_event`/`label` 三个 RPC 已经能写 JSONL（数据落盘正常），只是没对应的 Rust struct —— 加 struct 是为了 `SessionFile::load` 时能反序列化回强类型。

## 四、改动清单

按依赖顺序排，前面的不改完后面的编译不过。

> ✅ 全部完成。下面保留原始计划，每节末尾标注实际改动结果。

### 阶段 1：Message enum 扩展（核心）

#### 1.1 加 4 个新 Message struct

**文件：`ion-provider/src/types.rs`**，在 `ToolResultMessage` 之后、`Message` enum 之前插入：

```rust
/// Bash execution result (用户 `!cmd` 直发，或 bash 工具结果)
/// 对齐 pi BashExecutionMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BashExecutionMessage {
    pub role: String, // "bashExecution"
    pub command: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// 扩展自定义消息（content + display + details）
/// 对齐 pi CustomMessage<T>
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomMessage {
    pub role: String, // "custom"
    pub custom_type: String,  // 序列化为 "customType"
    pub content: CustomContent,
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub timestamp: i64,
}

/// CustomMessage.content 可以是字符串或 ContentBlock 数组
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// 分支摘要（回到主线时插入）
/// 对齐 pi BranchSummaryMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchSummaryMessage {
    pub role: String, // "branchSummary"
    pub summary: String,
    pub from_id: String,
    pub timestamp: i64,
}

/// 压缩摘要（compaction 后插入）
/// 对齐 pi CompactionSummaryMessage
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionSummaryMessage {
    pub role: String, // "compactionSummary"
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: i64,
}
```

> 注意：所有 struct 的 `role` 字段保持 `String` 类型（跟现有 UserMessage/AssistantMessage 一致），而不是用 enum —— 这样跟 pi 的弱类型对齐，序列化兼容性好。

#### 1.2 扩展 Message enum

**文件：`ion-provider/src/types.rs`**，改 `Message` enum：

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}
```

> **关键决策**：是否加 `#[serde(tag = "role")]`？
> - 加了：序列化变成 `{"role":"user", "content":[...]}`（扁平），跟 pi 一致
> - 不加：序列化变成 `{"User":{"role":"user","content":[...]}}`（嵌套），跟现状一致
>
> 现状是不加（ externally tagged，serde 默认）—— **保持现状，先不动序列化格式**，避免破坏现有 session.jsonl。新变体也用同样规则。后续如果要跟 pi 完全对齐 JSON 格式，再单独做一个迁移。

### 阶段 2：Provider 转换（让 LLM 看见）

#### 2.1 OpenAI provider 加 4 个 match arm

**文件：`ion-provider/src/provider/openai.rs:55-119`**

在 `Message::ToolResult` 之后加 4 个 arm，全部转成 `role: "user"`：

```rust
Message::BashExecution(b) => {
    if b.exclude_from_context == Some(true) {
        // `!cmd` 排除型不发给 LLM
        continue;
    }
    let mut text = format!("Ran `{}`\n```\n{}\n```", b.command, b.output);
    if b.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = b.exit_code {
        if code != 0 { text.push_str(&format!("\n\nCommand exited with code {code}")); }
    }
    if b.truncated {
        if let Some(ref p) = b.full_output_path {
            text.push_str(&format!("\n\n[Output truncated. Full output: {p}]"));
        }
    }
    openai_messages.push(OpenAIMessage {
        role: "user".into(), content: text,
        tool_call_id: None, tool_calls: None,
    });
}
Message::Custom(c) => {
    let text = match &c.content {
        CustomContent::Text(s) => s.clone(),
        CustomContent::Blocks(blocks) => blocks.iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            }).collect::<Vec<_>>().join("\n"),
    };
    openai_messages.push(OpenAIMessage {
        role: "user".into(), content: text,
        tool_call_id: None, tool_calls: None,
    });
}
Message::BranchSummary(b) => {
    let text = format!(
        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{}\n</summary>",
        b.summary
    );
    openai_messages.push(OpenAIMessage {
        role: "user".into(), content: text,
        tool_call_id: None, tool_calls: None,
    });
}
Message::CompactionSummary(c) => {
    let text = format!(
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
        c.summary
    );
    openai_messages.push(OpenAIMessage {
        role: "user".into(), content: text,
        tool_call_id: None, tool_calls: None,
    });
}
```

### 阶段 3：compact.rs token 计数

#### 3.1 加 4 个 match arm

**文件：`ion/src/agent/compact.rs:30-57`**

`msg_tokens` 是 exhaustive match，必须补：

```rust
Message::BashExecution(m) => (m.command.len() + m.output.len()) / 4,
Message::Custom(m) => match &m.content {
    CustomContent::Text(s) => s.len() / 4,
    CustomContent::Blocks(blocks) => blocks.iter()
        .map(|b| match b {
            ContentBlock::Text(t) => t.text.len() / 4,
            ContentBlock::Image(_) => 1000,
        }).sum(),
},
Message::BranchSummary(m) => m.summary.len() / 4,
Message::CompactionSummary(m) => m.summary.len() / 4,
```

### 阶段 4：非破坏性 match 位置（wildcard 已盖）

下面这些位置有 `_ =>` 通配符，**不会编译报错**，但新 variant 会被静默忽略。需要按场景判断是否补：

| 文件 | 行 | 用途 | 处理 |
|------|----|------|------|
| `src/bin/ion_worker.rs` | 300-309 | `get_session_stats` 统计 user/assistant/tool 数量 | 加 arm：bash/custom 计入 user 类 |
| `src/bin/ion_worker.rs` | 326, 415 | 取最后一条 assistant 文本 | 不变（新 variant 不是 assistant） |
| `src/bin/ion_worker.rs` | 562, 618, 622 | compact 时 token 统计 | 已在阶段 3 处理 |
| `src/bin/ion.rs` | 887-1044 | session 摘要统计 | 同上，加 arm |
| `src/bin/agent_demo.rs` | 54-105 | `describe_message` | 加 arm 以正确显示 |
| `src/worker/agent_worker.rs` | 80 | 取 assistant 文本 | 不变 |
| `src/rpc.rs` | 707 | 取 assistant 文本 | 不变 |

### 阶段 5：SessionEntry struct 补全

#### 5.1 加 4 个新 entry struct

**文件：`ion/src/session_jsonl.rs`**，在 `CustomEntry` 之后加：

```rust
/// CustomMessage entry (LLM 可见的扩展自定义消息)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomMessageEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "custom_message"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub customType: String,
    pub content: serde_json::Value, // string | (TextContent | ImageContent)[]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub display: bool,
}

/// System event entry (ION 原创设计)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemEventEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "system_event"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub customType: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub display: bool,
}

/// Label entry (书签)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "label"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub targetId: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Active tools change entry (工具集变更记录)
/// 对齐 pi ActiveToolsChangeEntry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveToolsChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "active_tools_change"
    pub id: String,
    pub parentId: String,
    pub timestamp: String,
    pub activeToolNames: Vec<String>,
}
```

#### 5.2 SessionFile::load 加 entry 分发

**文件：`ion/src/session_jsonl.rs:260-276`**

现状只识别 `"message"` 一种 entry type，其他都堆到 `entries: Vec<Value>`。**保持现状** —— 这些 entry 不需要回到 `messages: Vec<Message>` 里（它们不是对话消息）。struct 只是给将来 typed-access 用。

### 阶段 6：修复 `append_session_entry` 字段嵌套 bug

#### 6.1 问题

**文件：`ion/src/bin/ion_worker.rs:1248`**

当前实现：
```rust
let line = serde_json::json!({
    "type": entry_type,
    "id": ...,
    "parentId": sid,
    "timestamp": ...,
    "data": entry_data,   // ❌ 嵌套在 data 里
});
```

但 pi 的 JSONL 和 `session_jsonl.rs` 的 struct 都是**平铺字段**：
```json
{"type":"custom_message","customType":"...","content":"...","display":true,...}
```

#### 6.2 修复

把 `entry_data` 的字段合并到顶层：

```rust
fn append_session_entry(cwd: &str, sid: &str, entry_type: &str, entry_data: &serde_json::Value) {
    let path = session_jsonl::session_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 基础字段
    let mut line = serde_json::json!({
        "type": entry_type,
        "id": session_jsonl::generate_id(),
        "parentId": sid,
        "timestamp": session_jsonl::timestamp_iso(),
    });
    // 合并 entry_data 的字段到顶层（不嵌套）
    if let Some(obj) = entry_data.as_object() {
        if let Some(m) = line.as_object_mut() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}
```

### 阶段 7：补 `append_active_tools_change` RPC

**文件：`ion/src/bin/ion_worker.rs`**，在 `append_label` 之后：

```rust
"append_active_tools_change" => {
    let names: Vec<String> = params.get("activeToolNames")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect())
        .unwrap_or_default();
    append_session_entry(&worker_cwd, &sid, "active_tools_change", &serde_json::json!({
        "activeToolNames": names,
    }));
    output_response(&id, "append_active_tools_change",
        &serde_json::json!({"status":"appended","count":names.len()}));
}
```

### 阶段 8：测试

每个改动对应一条 CLI 测试命令。

| Test | 命令 | 期望 |
|------|------|------|
| T1 append_active_tools_change | `ion rpc --session x --method append_active_tools_change --params '{"activeToolNames":["bash","read"]}'` | JSONL 多一条 `active_tools_change`，字段平铺 |
| T2 字段平铺验证 | `cat session.jsonl \| grep active_tools_change` | 顶层有 `activeToolNames`，不在 `data` 里 |
| T3 custom_message 平铺 | `cat session.jsonl \| grep custom_message` | `customType/content/display` 在顶层 |
| T4 Message::BashExecution 序列化 | 构造一条 push 进 messages，get_messages | JSON 形如 `{"BashExecution":{...}}` |
| T5 provider 不报错 | 给 agent 喂 BashExecution 消息后 prompt | LLM 收到转换后的 user text，无 panic |
| T6 compact token 计数 | 给 agent 喂 4 种新消息，触发 compact | 不 panic，token 数合理 |
| T7 cargo build | `cargo build --bin ion-worker --bin ion` | 0 error |
| T8 cargo test --lib | `cargo test --lib` | 全部通过 |

## 五、后续工作（部分已完成）

- **不改 Message 序列化格式**：保持 externally-tagged（`{"User":{...}}`），不做 pi 风格的 internally-tagged 迁移。这是后续大改动。
- **✅ BashExecution 生产路径已部分实现**：`bash_command` RPC + `!cmd` 拦截已上线（参 [BASH_PLUGIN.md Part 1](./BASH_PLUGIN.md)），用户直发 bash 走 `Message::BashExecution`。LLM 调用的 `bash` 工具仍走 `ToolResult` 路径，后续切到 `BashExecution` 是 P3 工作。
- **✅ send_custom_message 已用 Message::Custom**：之前用 `Message::User` 的 bug 已修，现在插件异步通知走 `Message::Custom{role:"custom"}`，跟真实用户消息区分。
- **✅ save_worker_session 覆盖写 bug 已修**：之前 append 的 entry 在 worker 退出时被全量覆盖写冲掉，现在改成增量 append。
- **✅ Session Index 同步**：5 个 `append_*` RPC 现在同步更新 `sessions.index.json`，UI 可 O(1) 查询 last_thinking_level / last_active_tools / name / model / agent。
- **不动 dashboard 的 ChatMessage 类型**：那是独立 TS 类型，等内核稳定后再迁。
- **不加 `convert_to_llm` 集中函数**：ION 的 provider 边界（openai.rs）就是转换点，不引入额外抽象层。
- **CompactionSummary / BranchSummary 生产路径未接**：类型就绪，等 compact 改造 / 分支功能上线时再接入。

## 六、回滚策略

- 阶段 1-3 是核心，编译失败立即回滚
- 阶段 6（字段平铺）改变了 JSONL 写入格式，旧数据仍可读（`SessionFile::load` 用 `val["type"]` 取字段，对嵌套/平铺都兼容读取 message，其他 entry 当 raw Value 处理）
- 新增 RPC 不会破坏现有调用
