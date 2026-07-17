# Message Source Tag — 消息来源标记 设计文档

> **状态：开发中** — UserMessage 加 `source` 字段区分 prompt / steer / followUp / interrupt，让 UI 能渲染不同样式。

---

## 何时使用这个模板

启动新功能开发。本文档覆盖：能力清单 → 数据结构 → 主流程 → 接口规格 → CLI 测试。

**参考样本**：
- [docs/design/SOFT_DELETE_COMPACT.md](./SOFT_DELETE_COMPACT.md) — 软删除/软压缩内核机制（类似的 entry 元数据扩展）
- [docs/design/TURN_TIMELINE_UI.md](./TURN_TIMELINE_UI.md) — Turn Timeline UI 前端接口（本功能的消费方）

---

## 概览

当前 `Message::User` 不区分来源——正常 prompt、steer 插队、followUp 追加、interrupt 打断，四种消息进了 `self.messages` 后**长得一模一样**。UI 无法区分"这是打断"还是"正常输入"，无法渲染差异化的气泡样式。

本功能给 `UserMessage` 加 `source` 字段（`Option<String>`），在四个入口分别打标记。**静默注入**（记忆注入、system prompt 等）已有独立的 `custom` entry + `display` 机制，不重叠。

| 能力 | 入口 | 状态 |
|------|------|------|
| 正常 prompt 标记 `source:"prompt"` | `prompt` RPC（空闲时） | 🔧 待实现 |
| steer 插队标记 `source:"steer"` | `prompt` RPC（忙时 + behavior=steer）/ `steer` RPC | 🔧 待实现 |
| followUp 追加标记 `source:"followUp"` | `prompt` RPC（忙时 + behavior=followUp）/ `follow_up` RPC | 🔧 待实现 |
| interrupt 打断标记 `source:"interrupt"` | `prompt` RPC（忙时 + behavior=interrupt） | 🔧 待实现 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | UserMessage 加 source 字段 | 🔧 | `grep source ion-provider/src/types.rs` |
| 1.2 | 4 个入口打标记 | 🔧 | `cargo test --lib message_source` |
| 2.1 | list_turns / get_messages 返回 source | 🔧 | `ion rpc --method list_turns` |
| 2.2 | turn_summary 记录 source | 🔧 | jsonl 含 `"source":"steer"` |
| 3.1 | CLI 验证 4 种 source | 🔧 | `tests/message_source_ci.sh` |

---

## 1. 背景：四种消息模式（已有，但无标记）

**文件**：[src/bin/ion_worker.rs#L978-L991](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs#L978)

`prompt` RPC 的 `behavior` 参数已实现三种模式：

| behavior | 含义 | 实现机制 | 用户描述 |
|----------|------|---------|---------|
| `interrupt` | 直接打断当前 agent，立即执行 | `abort()` + 新 `run()` | "直接打断：当前如果有 best，也直接打断" |
| `steer` | 等当前 turn 结束，下个 turn 开始时插入 | `steering_queue` + `drain_steering()` | "等待插入：等它这一轮执行了之后，再插入" |
| `followUp` | 等 agent_end 后消费 | `follow_up_queue` + `outer_loop` | "等待结束：等到 agent end 触发再消费" |

加上空闲时的正常 `prompt`（无 behavior），共**四种来源**。

**问题**：这四种消息进了 `self.messages` 后都是普通 `Message::User`，落盘 jsonl 后：
```json
{"type":"message","message":{"User":{"role":"user","content":[{"Text":{"text":"先看 src/"}}]}}}
```
**完全看不出来源**。

---

## 2. 数据结构

### 2.1 UserMessage 加 source 字段

**文件**：[ion-provider/src/types.rs#L81-L85](file:///Users/xuyingzhou/Project/study-rust/ion-provider/src/types.rs#L81)

```rust
// 修改前
pub struct UserMessage {
    pub role: String,           // "user"
    pub content: Vec<ContentBlock>,
    pub timestamp: i64,
}

// 修改后
pub struct UserMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<MessageSource>,   // ← 新增
}
```

### 2.2 MessageSource 枚举

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum MessageSource {
    /// 正常发起新对话（prompt RPC 空闲时，无 behavior）
    Prompt,
    /// steer 插队：等当前 turn 结束，下个 turn 开始时注入（drain_steering）
    Steer,
    /// followUp 追加：等 agent_end 后消费（outer_loop 循环）
    FollowUp,
    /// interrupt 强行打断：abort 当前 agent + 立即新 run
    Interrupt,
}
```

序列化为驼峰：`"prompt"` / `"steer"` / `"followUp"` / `"interrupt"`。

### 2.3 为什么用 Option

- `Option::None` 时 `skip_serializing_if` 不序列化 → **向后兼容旧数据**（旧 jsonl 无 source 字段，反序列化时默认 None）
- 旧 session 读出来 `source=None`，UI 当作 `prompt` 处理（合理默认）

### 2.4 不做什么（明确边界）

| 不做 | 原因 |
|------|------|
| ~~agent_run_id 归组字段~~ | UI 用 `userContent` 非空的 turn 切分 agent run 即可，不需要额外字段 |
| ~~静默注入的 display 机制~~ | 已有 `custom` entry + `display:false`（见 `message_retrieval.rs:560`），记忆注入走那条路，与 message source 无关 |
| ~~给 AssistantMessage 加 source~~ | assistant 回复不需要区分来源（都是 agent 产生的） |

---

## 3. 主流程：四个入口打标记

### 3.1 prompt RPC（主入口）

**文件**：[src/bin/ion_worker.rs#L978](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs#L978)

```rust
"prompt" => {
    let text = params.get("text")...;
    let pbehavior = params.get("behavior")
        .and_then(|v| v.as_str())
        .unwrap_or("interrupt");

    // 根据 behavior + 运行状态决定 source
    let source = if !agent.is_running() {
        // 空闲时：无论什么 behavior，都是正常 prompt
        Some(MessageSource::Prompt)
    } else {
        match pbehavior {
            "steer"     => Some(MessageSource::Steer),
            "followUp"  => Some(MessageSource::FollowUp),
            _           => Some(MessageSource::Interrupt),  // interrupt 或默认
        }
    };

    // 构造 UserMessage 时带上 source
    let user_msg = Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent { text, text_signature: None })],
        timestamp: now_ms(),
        source,
    });
    // ... 后续按 behavior 分发（steer/followUp 入队列，interrupt 走 abort+run）
}
```

### 3.2 steer RPC（独立入口）

**文件**：[src/bin/ion_worker.rs#L1105](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs#L1105)

```rust
"steer" => {
    let text = params.get("text")...;
    let msg = Message::User(UserMessage {
        role: "user".into(),
        content: vec![...],
        timestamp: now_ms(),
        source: Some(MessageSource::Steer),   // ← 明确标记
    });
    agent.steer(msg);
}
```

### 3.3 follow_up RPC

**文件**：[src/bin/ion_worker.rs#L2610](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs#L2610)

```rust
"follow_up" => {
    let msg = Message::User(UserMessage {
        ...
        source: Some(MessageSource::FollowUp),
    });
    agent.follow_up(msg);
}
```

### 3.4 drain_steering（steer 消息注入点，无需改）

**文件**：[src/agent/agent_loop.rs#L1235](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L1235)

`drain_steering` 只是把 `steering_queue` 里的 Message push 到 `self.messages`，**source 标记在入队时已打好**，这里不用改。

### 关键决策点

| 场景 | 处理 |
|------|------|
| 空闲时发 prompt（无 behavior） | `source=Prompt` |
| 忙时 behavior=interrupt | `source=Interrupt`（abort + 新 run） |
| 忙时 behavior=steer | `source=Steer`（入 steering_queue） |
| 忙时 behavior=followUp | `source=FollowUp`（入 follow_up_queue） |
| 旧 session 无 source 字段 | 反序列化 `source=None`，UI 当 Prompt 处理 |

---

## 4. 接口规格

### 4.1 list_turns 返回（加 source）

每条 turn 的 `userContent` 旁边加 `source` 字段：

```json
{
  "turns": [{
    "turnId": "0",
    "userContent": "帮我分析架构",
    "source": "prompt",
    "assistantContent": "...",
    "durationMs": 3808
  }, {
    "turnId": "1",
    "userContent": "先只看 src/",
    "source": "steer",
    "assistantContent": "...",
    "durationMs": 2100
  }]
}
```

### 4.2 get_messages 返回（message 里带 source）

```json
{
  "messages": [{
    "type": "message",
    "message": {
      "User": {
        "role": "user",
        "content": [{"Text": {"text": "先看 src/"}}],
        "source": "steer"
      }
    }
  }]
}
```

### 4.3 jsonl 落盘格式

```json
{"type":"message","id":"...","message":{"User":{"role":"user","content":[{"Text":{"text":"先看 src/"}}],"source":"steer"}}}
```

`source` 为 `None` 时（旧数据或 Prompt）`skip_serializing_if` 不输出，保持 jsonl 紧凑。

---

## 5. UI 渲染建议

| source | 样式建议 | 说明 |
|--------|---------|------|
| `prompt` | 正常用户气泡 | 默认样式 |
| `steer` | 🟡 黄色左边框 + "插队" 小标签 | turn 间插入 |
| `followUp` | 🔵 蓝色左边框 + "追加" 小标签 | agent 结束后追加 |
| `interrupt` | 🔴 红色左边框 + "打断" 小标签 | 强行打断 |

---

## 6. CLI 测试指南

详见 [docs/testing/MESSAGE_SOURCE_CLI_TEST.md](../testing/MESSAGE_SOURCE_CLI_TEST.md)。

核心验证：
- 四种 source 都正确标记
- list_turns / get_messages 返回含 source
- 旧数据无 source 字段不报错（向后兼容）
