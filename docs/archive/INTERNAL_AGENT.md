# Internal Agent — 扩展内部 Agent 调用规范

> **状态**：定稿（2026-08-06）
> **目的**：定义扩展如何用「场景 1 完整能力」跑内部 agent loop（内存模式，不持久化）

---

## 0. 设计起源

### 为什么做这个

扩展（LearningExtension / GoalSupervisor / AutoSessionTitle）经常需要调 LLM。之前每个扩展自己造轮子：
- AutoSessionTitle：自己构造 Context + 调 registry::complete
- GoalSupervisor：自己构造 Context + 调 registry::complete（含 generate_goal_plan / generate_checks_via_llm）
- LearningExtension：调 run_skill_distillation（通过 session_id 读盘）

问题：
1. **能力碎片化**：有的扩展只能单轮（AutoSessionTitle），有的能多轮（GoalSupervisor），但没有统一接口
2. **没法传工具**：扩展调 LLM 时不能让 LLM 用 read/write/bash 做辅助操作
3. **没法限制循环**：单次 complete 没有 max_turns，LLM 可能无限循环
4. **要么污染原 session，要么走 session_id 读盘**：没有「内存快照」模式

### 设计原则

1. **完全对齐场景 1（cmd_run）的能力**：工具、循环、thinking、schema、retry 都支持
2. **内存模式优先**：不写 session.jsonl、不更新 SessionIndex、不触发其他扩展
3. **不污染原 session**：messages_snapshot 是 clone，原 agent.messages() 不变
4. **Builder 模式**：链式调用，必填项少，可选项多

---

## 1. API 规范

### 1.1 RunAgentRequest

```rust
pub struct RunAgentRequest {
    // ── 必填 ──
    pub tier: String,              // "fast" / "pro" / "max"（从 tier_models 解析）
    pub prompt: String,            // 用户 prompt

    // ── 可选（都有合理默认）──
    pub system_prompt: Option<String>,              // 覆盖 system prompt
    pub messages_snapshot: Option<Vec<Message>>,    // 续接上下文（clone）
    pub max_turns: Option<u64>,                     // 循环上限（None=无限）
    pub tools: Option<Vec<String>>,                 // 工具白名单（None=全部注册工具）
    pub thinking: Option<String>,                   // "off"/"minimal"/"low"/"medium"/"high"/"xhigh"
    pub json: bool,                                 // 强制 JSON 输出格式
    pub json_schema: Option<serde_json::Value>,     // JSON Schema 校验（implies json=true）
    pub schema_retries: u32,                        // Schema 校验失败重试次数（默认 3）
}
```

### 1.2 Builder 方法

```rust
RunAgentRequest::new(tier, prompt)           // 必填两项
    .with_system_prompt(sp)                   // 可选 system prompt
    .with_messages(msgs)                      // 续接上下文
    .with_max_turns(n)                        // 循环上限
    .with_tools(vec!["read", "grep"])         // 工具白名单
    .with_thinking("low")                     // thinking level
    .with_json_schema(schema)                 // JSON Schema（implies json=true）
```

### 1.3 RunAgentResult

```rust
pub struct RunAgentResult {
    pub output: String,           // 最终 assistant text
    pub messages: Vec<Message>,   // 完整对话历史（内存）
    pub turn_count: u64,          // 跑了几轮
    pub tool_call_count: u64,     // 调了几次工具
}
```

### 1.4 run_agent 函数

```rust
pub async fn run_agent(
    registry: &Arc<ApiRegistry>,
    tools: ToolRegistry,
    req: RunAgentRequest,
) -> Result<RunAgentResult, String>
```

---

## 2. 行为规范

### 2.1 必须做的

| # | 行为 | 说明 |
|---|---|---|
| M1 | 从 tier_models 解析 model | tier="fast" → 读 config.json tier_models["fast"] |
| M2 | tier 未配置时 fallback 到 default_model | 不 fail，用 default_provider + default_model |
| M3 | 从 providers[tier_provider].api_key 拿 api_key | 不走 env var（除非 config.json 也没配） |
| M4 | 支持 messages_snapshot | clone 后 prepend，不修改调用方的 messages |
| M5 | 支持 max_turns | 到了就停，即使 LLM 还想继续 |
| M6 | 支持 tools 白名单 | 只允许白名单里的工具 |
| M7 | 支持 thinking level | 透传给 LLM |
| M8 | 支持 JSON Schema 校验 | 输出不符合 schema 时 retry |
| M9 | 返回完整 messages | 调用方能看对话历史 |

### 2.2 必须不做的

| # | 禁止行为 | 理由 |
|---|---|---|
| N1 | 不写 session.jsonl | 内存模式，不持久化 |
| N2 | 不更新 SessionIndex | 内部调用，不产生 session 元数据 |
| N3 | 不触发 AutoSessionTitle | 内部 agent 不该生成标题 |
| N4 | 不触发 LearningExtension | 避免递归蒸馏 |
| N5 | 不注册额外扩展 | 调用方传 tools，不传 extensions |
| N6 | 不创建 follow_up channel | 内部 agent 不需要 background bash 通知 |
| N7 | 不 emit extension events | 不向 EventBus 广播 |

### 2.3 错误处理

| 场景 | 返回 |
|---|---|
| tier 不可解析（tier_models 没配 + default_model 没配） | `Err("tier 'X' not configured...")` |
| agent.run() 内部错误 | `Err("agent.run() failed: ...")` |
| Schema 校验全失败 | `Err("schema mismatch after N attempts: ...")` |
| 输出不是有效 JSON（schema 模式下） | `Err("output is not valid JSON: ...")` |

---

## 3. 使用场景

### 场景 A：聊天总结（单轮，无工具）

```rust
let snapshot = agent.messages().to_vec();
let result = run_agent(
    &registry,
    tools_registry,
    RunAgentRequest::new("fast", "Summarize the key decisions in this conversation")
        .with_messages(snapshot)
        .with_max_turns(1),               // 单轮就够
).await?;
// result.output 是总结文本
```

### 场景 B：Skill 蒸馏（多轮 + 读工具）

```rust
let result = run_agent(
    &registry,
    tools_registry,
    RunAgentRequest::new("pro", "Distill a reusable skill from this session")
        .with_messages(agent.messages().to_vec())
        .with_max_turns(10)                // 允许多轮
        .with_tools(vec!["read", "grep"])  // 能读代码辅助分析
        .with_system_prompt("You are a skill distiller..."),
).await?;
```

### 场景 C：结构化数据提取（Schema 约束）

```rust
let schema = json!({
    "type": "object",
    "properties": {
        "title": {"type": "string"},
        "priority": {"type": "string", "enum": ["high", "medium", "low"]},
        "tasks": {"type": "array", "items": {"type": "string"}}
    },
    "required": ["title", "priority"]
});

let result = run_agent(
    &registry,
    tools_registry,
    RunAgentRequest::new("fast", "Extract action items from this conversation")
        .with_messages(snapshot)
        .with_max_turns(1)
        .with_json_schema(schema),
).await?;
let parsed: serde_json::Value = serde_json::from_str(&result.output)?;
```

### 场景 D：Goal 验证（多轮 + 全工具）

```rust
let result = run_agent(
    &registry,
    tools_registry,
    RunAgentRequest::new("max", "Verify the goal is achieved by running cargo test")
        .with_max_turns(15)               // 允许长循环
        .with_tools(vec!["bash", "read", "write", "edit"])  // 能改代码 + 跑测试
        .with_thinking("medium"),
).await?;
```

---

## 4. 对齐场景 1（cmd_run）能力矩阵

| 场景 1 能力 | run_agent 支持 | 说明 |
|---|---|---|
| 多轮 agent loop | ✅ | 内置（agent.run） |
| max_turns | ✅ | req.max_turns |
| 工具白名单 | ✅ | req.tools |
| 工具黑名单 | ❌ | 暂不支持（用白名单替代） |
| Thinking level | ✅ | req.thinking |
| JSON 输出 | ✅ | req.json |
| JSON Schema | ✅ | req.json_schema |
| Schema retry | ✅ | req.schema_retries |
| Skills 加载 | ❌ | 内部 agent 不加载 skills |
| Extensions | ❌ | 内部 agent 不注册扩展 |
| session.jsonl | ❌ | **设计决定：不写** |
| SessionIndex | ❌ | **设计决定：不更新** |
| follow_up channel | ❌ | 内部 agent 不需要 |
| compact | ✅ | AgentConfig.enable_compact=true（默认） |

---

## 5. 实现清单

| 组件 | 文件 | 状态 |
|---|---|---|
| `RunAgentRequest` struct | `src/internal_agent.rs` | ✅ |
| Builder 方法 | `src/internal_agent.rs` | ✅ |
| `RunAgentResult` struct | `src/internal_agent.rs` | ✅ |
| `run_agent()` 函数 | `src/internal_agent.rs` | ✅ |
| `Agent::set_messages()` | `src/agent/agent_loop.rs` | ✅ |
| 模块注册 | `src/lib.rs` | ✅ |
| Schema retry 循环 | `src/internal_agent.rs` | ⚠️ 基础版（单次校验，不是多轮 retry） |
| CLI 测试入口（`ion run_agent`） | `src/bin/ion.rs` | ❌ 未加 |

---

## 6. 后续优化

| 优先级 | 内容 |
|---|---|
| P0 | Schema retry 循环（当前只单次校验，应该 retry schema_retries 次） |
| P1 | CLI `ion run_agent` 子命令（类似 `ion query` 但跑完整 loop） |
| P1 | LearningExtension 迁移到 run_agent |
| P2 | GoalSupervisor 迁移到 run_agent |
| P2 | 支持 tool 黑名单（exclude_tools） |
| P3 | 异步版本（spawn + callback） |
