# FauxProvider 设计文档

> **状态：设计稿** — 架构级 LLM Mock，对标 pi `@dyyz1993/pi-ai` 的 `FauxProvider`。
>
> 这是 Session Tree 验收 harness 的基础设施，也是未来所有需要"快速验证 agent 行为"场景的共享组件。

---

## 0. 这个东西是什么（一句话）

一个**注册在 `ApiRegistry` 里的假 provider**。当 model 的 `api` 字段配成 `"faux"` 时，agent loop 调用 `registry::stream()` 会派发到它，它从一个预设的响应队列里 FIFO 取出响应，伪装成真实 LLM 的流式返回。

### 0.1 对标 pi

pi 的 `FauxProvider` 在 `packages/ai/src/providers/faux.ts`（499 行），是发布在 `@dyyz1993/pi-ai` 里的**共享基础设施**，被 agent / coding-agent / tui 三个包的测试复用。

ION 的 FauxProvider 对标它的核心设计：
- ✅ FIFO 队列 + 队列空强制报错（loud failure）
- ✅ 工厂函数响应（一等公民，能根据 context 动态返回）
- ✅ 真正的 token 分块流式
- ✅ Builder 函数集（`faux_text` / `faux_thinking` / `faux_tool_call` / `faux_assistant_message`）

**两期落地**：
- **Phase 1（本期）**：纯编程接口，对齐 pi——只能在 Rust 测试代码里用
- **Phase 2（后续）**：加 CLI flag（`--faux-script`），让开发时也能手动跑

### 0.2 核心原理（无技术细节）

ION 调 LLM 的方式是："根据 model 配置，找到对应的 provider，调它的 `stream()` 方法"。

FauxProvider 就是**多注册一个叫 `"faux"` 的 provider**。当你用 `--model faux/test-model` 时（或测试里把 model 的 api 设成 `"faux"`），agent 找到的就是它，它从一个队列里吐出你预设好的响应（文本、工具调用、思考等），和真 LLM 的输出格式完全一样。

```
真实链路：                          Faux 链路：
agent → registry → OpenAI provider   agent → registry → FauxProvider
                  ↓                                  ↓
              HTTP → 真实 LLM                  队列 → 预设响应
                  ↓                                  ↓
              session.jsonl                     session.jsonl（一模一样）
```

**关键：session.jsonl 的写入路径完全不变。** agent loop 不知道、也不需要知道响应来自真 LLM 还是 faux。

---

## 1. 为什么需要它

ION 现在的测试要么：
- **绕过 agent loop**（`compaction_e2e.rs` 手写 JSONL）——快，但跳过了真实链路，无法测"回滚后继续对话"等需要 agent 续轮的场景
- **调真实 LLM**（`#[ignore]` + 真 API key）——真实，但慢、贵、不确定
- **mock worker**（`mock_worker.rs`）——只 mock 了 worker 进程协议，没 mock LLM

FauxProvider 填补中间的空缺：**走完整 agent 链路，但响应是预设的**。

### 1.1 解决的场景

| 场景 | 之前怎么做 | 用 FauxProvider 后 |
|------|-----------|-------------------|
| 验证分支/回滚写对了 session.jsonl | 手写 JSONL，跳过 agent | faux 回预设消息，agent 真的写 jsonl |
| 验证回滚后"继续对话" | 做不到（要真 LLM） | faux 预设下一轮响应，agent 续轮 |
| 验证工具调用编排（spawn_worker 等） | 要么跳过，要么真 LLM | faux 回 tool_call，验证编排逻辑 |
| 断言 agent 发给 LLM 的 context | 做不到 | 工厂函数能拿到 context |
| CI 跑 agent 行为 | 全 `#[ignore]` | CI 能跑 agent 全链路 |
| 回归测试 | 依赖真 API 稳定性 | 完全确定 |

---

## 2. 架构

### 2.1 在 ION 里的位置

```
┌─────────────────────────────────────────────────────┐
│  tests/session_tree_harness.rs（及其他测试）          │
│    use ion_provider::faux::*;                       │
│    let faux = FauxProvider::new();                  │
│    faux.set_responses(vec![...]);                   │
└──────────────────────┬──────────────────────────────┘
                       │ 编程接口（Phase 1）
                       ▼
┌─────────────────────────────────────────────────────┐
│  ion-provider crate                                 │
│    ApiRegistry                                      │
│      "openai-completions" → OpenAICompletionsProvider│
│      "anthropic-messages" → AnthropicMessagesProvider│
│      "faux" → FauxProvider ← 新增                    │
│                                                     │
│    dispatch: registry::stream() 看 model.api 路由    │
└──────────────────────┬──────────────────────────────┘
                       │ model.api == "faux"
                       ▼
┌─────────────────────────────────────────────────────┐
│  FauxProvider (新增 src/faux.rs)                     │
│    内部: 响应队列 (Mutex<VecDeque<FauxResponseStep>>) │
│    stream(): shift 一个响应 → 构造 EventStream        │
│    支持: 静态 AssistantMessage / 工厂函数             │
│    队列空 → 强制报错 "no more faux responses queued"  │
└─────────────────────────────────────────────────────┘
```

### 2.2 与现有架构的对齐（零侵入）

| 现有机制 | FauxProvider 关系 |
|---------|-------------------|
| `ApiProvider` trait（1 个方法 `stream`） | FauxProvider 实现这个 trait |
| `ApiRegistry.register(api, provider)` | `register("faux", ...)` |
| `Model.api` 路由字段 | model 配 `api: "faux"` |
| `EventStream` (mpsc) + `StreamEvent` enum | FauxProvider 用相同的机制发事件 |
| `complete()` 自由函数 | 不用改，内部调 `stream()`，FauxProvider 的 `sender.end()` 让它工作 |
| API key resolve | FauxProvider 不调 `resolve_api_key`，天然免 key |

**不修改任何现有 provider、不改 dispatch 逻辑、不改 agent loop。**

---

## 3. 核心数据结构

### 3.1 `FauxResponseStep`（对标 pi 的 `FauxResponseStep`）

预设响应有两种形态——**静态消息**或**工厂函数**：

```rust
/// 一条预设响应。按顺序从队列消费（FIFO）。
pub enum FauxResponseStep {
    /// 静态：直接返回这个 AssistantMessage
    Static(AssistantMessage),

    /// 动态：根据 agent 发来的 context 决定返回什么
    /// 对标 pi 的 FauxResponseFactory
    Factory(Box<dyn Fn(&Context, Option<&StreamOptions>, &FauxState, &Model)
              -> AssistantMessage + Send + Sync>),
}

/// FauxProvider 的可观测状态（传给工厂函数）
pub struct FauxState {
    pub call_count: usize,    // 第几次调用（从 0 开始）
}
```

**工厂函数的价值**（pi 的核心设计，一等公民）：
- 能看到 agent 发给 LLM 的完整 `Context`（messages、tools 等）
- 能根据 `call_count` 做"第 N 次返回 X"的分支
- 让测试能**断言 agent 发给 LLM 什么**——比如"第 3 轮 context 里应该有 memory 注入"

### 3.2 `FauxProvider` struct

```rust
pub struct FauxProvider {
    /// FIFO 响应队列，Mutex 保护（Send + Sync 要求）
    queue: Mutex<VecDeque<FauxResponseStep>>,
    /// 调用计数（工厂函数可读）
    call_count: AtomicUsize,
}

impl FauxProvider {
    pub fn new() -> Self { /* 空队列 */ }

    /// 替换整个队列（对标 pi setResponses）
    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) { /* lock + clear + extend */ }

    /// 追加到队列（对标 pi appendResponses）
    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) { /* lock + extend */ }

    /// 队列剩余数量（对标 pi getPendingResponseCount）
    pub fn pending_count(&self) -> usize { /* lock + len */ }

    /// 调用次数（对标 pi state.callCount）
    pub fn call_count(&self) -> usize { /* AtomicUsize::load */ }
}
```

### 3.3 `impl ApiProvider`（唯一的 trait 方法）

```rust
#[async_trait]
impl ApiProvider for FauxProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        // 1. FIFO shift 一个响应
        let step = self.queue.lock().unwrap().pop_front()
            .ok_or_else(|| ProviderError::Other(
                "No more faux responses queued".into()  // ← loud failure，对齐 pi
            ))?;
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);

        // 2. 解析响应（静态 or 工厂）
        let message = match step {
            FauxResponseStep::Static(msg) => msg,
            FauxResponseStep::Factory(f) => f(context, options, &FauxState{call_count: count}, model),
        };

        // 3. 构造 EventStream，逐块发事件（对标 pi streamWithDeltas）
        let (mut stream, sender) = EventStream::new();
        let model_clone = model.clone();
        tokio::spawn(async move {
            sender.push(StreamEvent::Start { partial: message.clone() });
            // 对每个 content block 发对应事件序列（见 §3.4）
            faux_stream_blocks(&sender, &message, &model_clone).await;
            sender.end(message);  // ← 触发 oneshot，让 complete() 能拿到
        });

        Ok(stream)
    }
}
```

### 3.4 流式分块（对标 pi `streamWithDeltas`）

FauxProvider **不一次发完整消息**，而是模拟真实流式：把文本切成 token 粒度的 chunk，逐个发 `TextDelta`。

```rust
/// 把 AssistantMessage 的 content blocks 逐个流式发送
async fn faux_stream_blocks(sender: &EventSender, message: &AssistantMessage, _model: &Model) {
    for (idx, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Text { text } => {
                sender.push(StreamEvent::TextStart { content_index: idx, partial: message.clone() });
                // 切成 ~4 字符的 chunk（对标 pi splitStringByTokenSize）
                for chunk in split_by_token_size(text, 3, 5) {
                    sender.push(StreamEvent::TextDelta { content_index: idx, delta: chunk, partial: message.clone() });
                    // queueMicrotask 等价：yield 一次（不真延迟，除非配了 tokensPerSecond）
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::TextEnd { content_index: idx, content: text.clone(), partial: message.clone() });
            }
            ContentBlock::ToolCall { name, arguments, .. } => {
                sender.push(StreamEvent::ToolCallStart { content_index: idx, partial: message.clone() });
                // 把 arguments JSON 也分块流（对标 pi）
                let args_str = serde_json::to_string(arguments).unwrap_or_default();
                for chunk in split_by_token_size(&args_str, 3, 5) {
                    sender.push(StreamEvent::ToolCallDelta { content_index: idx, delta: chunk, partial: message.clone() });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::ToolCallEnd { content_index: idx, tool_call: /* rebuild */, partial: message.clone() });
            }
            ContentBlock::Thinking { thinking } => {
                // 同 Text，发 ThinkingStart/Delta/End
            }
        }
    }
}

/// 把字符串切成 min-max 字符的随机大小 chunk（对标 pi）
fn split_by_token_size(s: &str, min: usize, max: usize) -> Vec<String> { /* rand chunking */ }
```

**关键：FauxProvider 不执行工具**——它只发"模型请求调用工具"的事件。工具的实际执行由 agent loop 负责（和真实 LLM 一样）。

---

## 4. Builder 函数（对标 pi `fauxText` / `fauxToolCall` 等）

让测试代码可读，对标 pi 的 builder 集：

```rust
/// 文本 block
pub fn faux_text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

/// 思考 block
pub fn faux_thinking(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking { thinking: thinking.into() }
}

/// 工具调用 block（id 自动生成）
pub fn faux_tool_call(name: &str, arguments: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: format!("call_{}", generate_id()),
        name: name.into(),
        arguments,
    }
}

/// 构造完整 AssistantMessage
/// content 可以是：字符串（自动包成 faux_text）、单个 block、或 block 数组
pub fn faux_assistant_message(
    content: FauxContent,
    options: FauxMessageOptions,
) -> AssistantMessage {
    let mut msg = AssistantMessage::new(/* faux model */);
    msg.content = match content {
        FauxContent::Text(s) => vec![faux_text(&s)],
        FauxContent::Single(b) => vec![b],
        FauxContent::Many(v) => v,
    };
    msg.stop_reason = options.stop_reason.unwrap_or(StopReason::EndTurn);
    msg.usage = Usage::zero();  // faux 不计真实 token
    msg.api = "faux".into();
    msg.provider = "faux".into();
    msg.model = "faux-1".into();
    msg
}

pub enum FauxContent {
    Text(String),
    Single(ContentBlock),
    Many(Vec<ContentBlock>),
}

pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub error_message: Option<String>,  // 测试 error 路径
}
```

---

## 5. 注册与路由

### 5.1 注册到 ApiRegistry

**文件**：`ion-provider/src/registry.rs`（`register_builtins` 附近）

FauxProvider **不进 `register_builtins`**（那是真实 provider）。测试代码自己注册：

```rust
// 在测试 harness 里
let mut registry = ApiRegistry::new();
registry.register_builtins();  // 真实 provider
let faux = Arc::new(FauxProvider::new());
registry.register("faux", Box::new(FauxProviderHandle(Arc::clone(&faux))));
```

> **注意**：`ApiProvider` 注册的是 `Box<dyn>`，但测试需要拿到 FauxProvider 的控制柄（调 `set_responses`）。
> 解决：注册一个 thin wrapper 持有 `Arc<FauxProvider>`，`stream()` 委托给内部。

### 5.2 路由到 faux

测试 harness 构造 Agent 时，把 model 的 `api` 设成 `"faux"`：

```rust
let faux_model = Model {
    id: "faux-1".into(),
    api: "faux".into(),       // ← 关键，dispatch 靠这个
    provider: "faux".into(),
    // 其他字段默认/零
    ..Default::default()
};
```

agent loop 调 `registry::stream(registry, model, ...)` 时，`registry.get("faux")` 拿到 FauxProvider，自动派发。

### 5.3 免 API key

FauxProvider 的 `stream()` **从不调用 `resolve_api_key`**——key 解析是各 provider 内部的事，FauxProvider 不碰。所以无需配 key。

---

## 6. Session Tree Harness（基于 FauxProvider）

### 6.1 Harness 设计哲学（对标 pi：暴露原始对象）

pi 的测试 harness 不封装方法，而是**返回一堆原始对象的引用**（session、sessionManager、events 数组）。测试直接操作这些对象。

ION 的 SessionTreeHarness 同样设计：

```rust
/// Session Tree 测试 harness
/// 对标 pi packages/coding-agent/test/suite/harness.ts
pub struct SessionTreeHarness {
    /// 暴露：测试直接读写 entries
    pub session_file: SessionFile,
    pub session_id: String,
    pub file_path: PathBuf,

    /// FauxProvider 控制柄（排响应、查 call_count）
    pub faux: Arc<FauxProvider>,

    /// 事件收集（扩展钩子触发的事件）
    pub events: Arc<Mutex<Vec<Value>>>,

    /// 临时目录（Drop 时自动清理）
    _tmp: TempDir,
}

impl SessionTreeHarness {
    /// createHarness 等价：临时目录 + 注册 faux + 空会话
    pub fn create() -> Self { /* ... */ }

    /// 从文件加载（模拟重启）
    pub fn load_from(path: &Path) -> Self { /* ... */ }

    /// seed：走真实 agent loop，让消息沉淀到 session.jsonl
    /// responses 是 faux 队列（一次 prompt 消费一个）
    pub async fn seed(&self, user_text: &str, responses: Vec<FauxResponseStep>) {
        self.faux.set_responses(responses);
        // 调用 agent loop（注入 faux registry）
        // 消息真实写入 session_file
    }

    /// cleanup：Drop 自动触发（TempDir）
}
```

**关键：不提供 `tree()`/`branches()` 等封装方法。** 测试直接调 `session_file.get_tree()`、`session_file.current_leaf()`——和 pi 一样，harness 是"引用集合"，不是"方法封装层"。

### 6.2 用法示例（对标 pi 测试套路）

```rust
#[tokio::test]
async fn branch_preserves_old_path() {
    // 1. create + push 到清理数组（对标 pi harnesses.push）
    let harness = SessionTreeHarness::create();
    let _guard = harness.scope_guard();  // RAII cleanup

    // 2. seed：用 faux 走真实 agent，造 3 轮对话
    harness.seed("实现加法", vec![
        faux_step(faux_assistant_message("fn add", Default::default())),
    ]).await;
    harness.seed("加日志", vec![
        faux_step(faux_assistant_message("已加 println", Default::default())),
    ]).await;

    // 3. 操作：直接调 SessionFile 方法（harness 暴露原始对象）
    let entry_id = /* 找到 "已加 println" 的 entry id */;
    harness.session_file.branch(&entry_id, Some("try-div")).unwrap();

    // 4. 断言：直接查 SessionFile（harness 不封装）
    let tree = harness.session_file.get_tree();
    assert!(/* tree 显示 msg_004 有两个子节点 */);
    assert_eq!(harness.session_file.current_leaf(), Some(/* try-div 的 leaf */));

    // 5. 回滚后继续——仍能用 faux 续轮
    harness.seed("加除法", vec![
        faux_step(faux_assistant_message("fn div", Default::default())),
    ]).await;

    // 6. cleanup 自动（_guard Drop）
}
```

### 6.3 工厂函数响应（断言 agent 发给 LLM 什么）

这是 pi FauxProvider 的杀手锏，ION 也支持：

```rust
harness.seed("第3轮", vec![
    FauxResponseStep::Factory(Box::new(|ctx, _opts, state, _model| {
        // 能看到 agent 发给 LLM 的完整 context！
        let msg_count = ctx.messages.len();
        let has_memory = ctx.messages.iter().any(|m| {
            /* 检查是否有 memory 注入的 XML */
        });
        // 根据 context 动态返回
        faux_assistant_message(
            FauxContent::Text(format!("看到 {} 条消息, memory={}", msg_count, has_memory)),
            Default::default(),
        )
    })),
]).await;
```

这让验收能断言"回滚到某分支后，agent 发给 LLM 的 context 只含该分支路径"——纯静态响应做不到。

---

## 7. 验收用例（FauxProvider 自身）

> FauxProvider 是基础设施，自己也要验收。对标 pi `test/faux-provider.test.ts`。

### P0

| # | 用例 | 操作 | 预期 |
|---|------|------|------|
| F-P0.1 | 基本文本回放 | set 1 个 text 响应，prompt 一次 | session.jsonl 有对应 assistant 消息 |
| F-P0.2 | 多步 FIFO | set 3 个响应，prompt 3 次 | 每次 prompt 拿对应响应，call_count==3 |
| F-P0.3 | 工具调用回放 | set tool_call 响应 | agent 真执行工具（工具是测试注入的真实现） |
| F-P0.4 | 工厂函数响应 | set Factory 响应 | 工厂能拿到 context，按 context 返回 |
| F-P0.5 | 流式事件完整 | subscribe 观察 | Start→TextDelta*N→TextEnd→Done |
| F-P0.6 | 免 API key | 不设 key | 不报错 |

### P1

| # | 用例 | 预期 |
|---|------|------|
| F-P1.1 | 队列空强制报错 | prompt 时队列已空 → `ProviderError "No more faux responses queued"` |
| F-P1.2 | call_count 递增 | 每次调用 +1，工厂能读到 |
| F-P1.3 | appendResponses 追加 | 不清空原队列 |
| F-P1.4 | error 路径 | `stop_reason: Error` 响应 → agent 走错误处理 |
| F-P1.5 | 不污染真实 provider | 不配 faux 时，openai/anthropic 正常 dispatch |

---

## 8. 实现顺序

### Phase 1：纯编程接口（本期，对齐 pi）

| Step | 内容 | 预估 |
|------|------|------|
| 1 | `FauxResponseStep` enum（Static + Factory）+ `FauxState` | 0.5 天 |
| 2 | `FauxProvider` struct + 队列管理 + `impl ApiProvider` | 1 天 |
| 3 | 流式分块（`faux_stream_blocks` + `split_by_token_size`） | 0.5 天 |
| 4 | Builder 函数（`faux_text`/`faux_tool_call`/`faux_assistant_message`） | 0.5 天 |
| 5 | SessionTreeHarness（暴露原始对象 + seed + TempDir） | 0.5 天 |
| 6 | FauxProvider 自验收（§7） | 0.5 天 |
| **合计** | | **~3.5 天** |

### Phase 2：CLI 集成（后续）

| Step | 内容 | 预估 |
|------|------|------|
| 1 | `--faux-script <path>` flag（脚本 JSONL → FauxResponseStep） | 0.5 天 |
| 2 | `--faux-reply <text>` 快捷单条 | 0.25 天 |
| 3 | 子 worker 环境变量继承（`ION_FAUX_SCRIPT`） | 0.5 天 |
| 4 | 脚本格式文档化 | 0.25 天 |
| **合计** | | **~1.5 天** |

---

## 9. 与 pi 的对齐核查

| pi 特性 | ION 对应 | 状态 |
|---------|---------|------|
| `FauxProvider`（`@dyyz1993/pi-ai`） | `ion_provider::faux::FauxProvider` | 🔧 设计 |
| `registerFauxProvider` | `registry.register("faux", ...)`（测试 harness 调） | 🔧 |
| `FauxResponseStep = AssistantMessage \| Factory` | `FauxResponseStep::Static \| Factory` | 🔧 |
| FIFO shift + loud failure | `pop_front` + `ProviderError` | 🔧 |
| `streamWithDeltas`（token 分块） | `faux_stream_blocks` + `split_by_token_size` | 🔧 |
| `fauxText`/`fauxThinking`/`fauxToolCall`/`fauxAssistantMessage` | `faux_text`/`faux_thinking`/`faux_tool_call`/`faux_assistant_message` | 🔧 |
| `state.callCount` | `AtomicUsize` + `FauxState.call_count` | 🔧 |
| `setResponses`/`appendResponses`/`getPendingResponseCount` | `set_responses`/`append_responses`/`pending_count` | 🔧 |
| Harness 暴露原始对象（session/sessionManager） | `SessionTreeHarness` 暴露 `session_file`/`faux`/`events` | 🔧 |
| **CLI flag 驱动** | pi 无；ION Phase 2 加（`--faux-script`） | ⚠️ 差异（ION 更重） |
| **Prompt cache 模拟** | pi 有；ION 暂不做（Usage 全零） | ⚠️ 差异（后续补） |

---

## 10. 在 Session Tree 验收里的角色

Session Tree 的 harness（`docs/testing/SESSION_TREE_SPEC.md`）基于本 FauxProvider：

```
SessionTreeHarness（验收外壳）
   ├─ 暴露原始对象：session_file / faux / events
   ├─ seed() → 用 FauxProvider 走真实 agent loop
   └─ branch/checkout/rollback → 直接调 SessionFile 方法

FauxProvider（本文档，基础设施）
   └─ 让 seed() 走 agent loop 但不联网
```

**演进路径：**
1. **Phase 1**：FauxProvider 编程接口上线 + SessionTreeHarness 接入
2. **Phase 2**：FauxProvider CLI flag 上线，开发时也能 `--faux-script` 手动跑
3. **Phase 3**：FauxProvider 成为全项目共享基础设施（Worker 恢复、Memory v0.2 验收也用）

---

## 11. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | Phase 2：CLI flag（`--faux-script`/`--faux-reply`） | P1 |
| 2 | 子 worker 环境变量继承（`ION_FAUX_SCRIPT`） | P1（随 Phase 2） |
| 3 | Prompt cache 模拟（对齐 pi，让 cost tracking 可测） | P2 |
| 4 | 录制/回放（`--record` 真实 LLM → 脚本） | P2 |
| 5 | 可配延迟（`tokensPerSecond`，测流式 UI） | P3 |
| 6 | 全局共享 harness 包（多 crate 复用时） | P3 |
