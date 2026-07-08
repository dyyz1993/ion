# Record/Replay 设计文档

> **状态：设计稿** — 录制/回放系统，环境变量驱动录制、`--model replay/<id>` 回放。复用已实现的 FauxProvider 作为回放引擎。让"复现某个真实会话"变成一行命令。

---

## ⚠️ 硬边界声明（读本文档前必读）

**Record/Replay 是 LLM 决策回放系统，不是完整环境回放系统。**

它只保证：**LLM 决策序列一致**——模型当时说了什么、调了什么工具、按什么顺序调。

它**不保证**：
- ❌ 工具结果一致（回放时 read/编辑/bash 真实执行，结果可能和录制时不同）
- ❌ 文件状态一致（回放时文件系统是当前状态，不是录制时的状态）
- ❌ 网络结果一致（HTTP 工具的响应是实时的）
- ❌ 时间状态一致
- ❌ 副作用一致

**一句话：Provider 层负责伪造模型输出，Runtime 层的工具执行是真的，不能绕过。**

这条边界决定了 Record/Replay 的定位（见 §12）：它是测试基础设施，不是虚拟机。用户如果误解成"完整复现"，会在回放时遭遇意外副作用（见 §4 安全风险）。

---

## 0. 这个东西是什么（一句话）

录制：用一个环境变量让正常使用的 `ion` 把每次 LLM 响应写到磁盘。回放：用 `--model replay/<录制ID>` 让 ion 不联网地按录制顺序回放那些响应——agent 的每一个决策（说什么、调什么工具）都被精确复现，只有工具的实际副作用（文件读写等）是真的。

### 0.1 你能用它做什么

```bash
# 录制：正常用，自动存
ION_RECORD=fix-bug-2026-07-08 ion --model glm-4.6 "修复 calc 的 bug"
# 进程退出后，录制在 ~/.ion/recordings/fix-bug-2026-07-08/trace.jsonl

# 回放：不联网，免 key，复现整个会话的 LLM 决策
ion --model replay/fix-bug-2026-07-08 "修复 calc 的 bug"

# 回放时换模型（语义上 replay/xxx 已经指定了，但你可以用任意 provider 前缀做标记）
ion --model replay/fix-bug-2026-07-08 "修复 calc 的 bug"
```

### 0.2 为什么工具调用会被自动捕获

关键原理：**录制的是 LLM 的每一轮响应，而工具调用是 LLM 响应的一部分。**

```
录制时会话的 LLM 调用序列：

第 1 轮：agent 发 context → glm-4.6 回 "我读文件" + tool_call(read, Cargo.toml)
        → 录下这条完整响应
agent 执行 read → 文件内容进 context（这是真的，不录）

第 2 轮：agent 发 context → glm-4.6 回 "用 tokio，我改" + tool_call(edit, ...)
        → 录下这条完整响应
agent 执行 edit → 文件改了（这是真的，副作用发生）

第 3 轮：agent 发 context → glm-4.6 回 "改完了"
        → 录下这条响应
```

回放时：3 轮 LLM 调用依次拿录制的 3 条响应。工具真实执行（read/edit），工具结果进 context，但 **LLM 的响应是回放的**。所以整个会话的"LLM 决策"完全复现——说什么、调什么工具、什么顺序——只有文件系统等副作用是真的。

**这就是"所有细节都被安排好"的含义：LLM 的每一步决策都是录制的，不可改变。**

---

## 1. 对标与关系

### 1.1 业界叫什么

这种模式业界有多个名字：
- **VCR**（Ruby 的 vcr gem 首创）—— 录制 HTTP 交互，回放
- **Cassette**（pytest-vcr、VCR.js）—— 录制文件叫磁带
- **Record/Replay**（通用术语）

ION 采用 Record/Replay 这个名字，最直白。

### 1.2 和已实现的 FauxProvider 的关系

Record/Replay 不是推翻 FauxProvider，是**在它上面加个录制器 + 自动加载**：

```
┌─────────────────────────────────────────────────────┐
│  Record/Replay 系统                                  │
│                                                     │
│  ┌──────────────┐    录制    ┌──────────────────┐  │
│  │ Recording    │ ────────→ │ ~/.ion/recordings│  │
│  │ Provider     │           │  /<id>/trace.jsonl│  │
│  │ (包真实模型)  │           └────────┬─────────┘  │
│  └──────────────┘                    │             │
│                                      │ 回放加载     │
│                                      ▼             │
│  ┌──────────────┐    FIFO    ┌──────────────────┐  │
│  │ Replay       │ ←──────── │ load_script()    │  │
│  │ Provider     │           │ (已实现)          │  │
│  │ (=FauxProvider│          └──────────────────┘  │
│  │  + 自动加载)  │                                 │
│  └──────────────┘                                 │
└─────────────────────────────────────────────────────┘
```

| | FauxProvider（已实现） | Record/Replay（本文档） |
|---|---|---|
| **响应来源** | 手工写 JSONL 脚本 | 自动录制真实会话 |
| **回放机制** | FIFO 队列 | **复用** FauxProvider |
| **触发** | `ION_FAUX_SCRIPT` / `ION_FAUX_REPLY` | `--model replay/<id>` |
| **格式** | 手写 JSONL | **同格式**（`trace.jsonl` 可被 `load_script` 直接读） |
| **适合** | "如果 LLM 这么回，agent 会怎样" | "复现上次那个真实会话" |

**关键复用：** 回放 = FauxProvider + 从 `~/.ion/recordings/<id>/trace.jsonl` 自动加载。录制文件的格式和 faux 脚本一致，`load_script` 已经能解析。

---

## 2. 用户怎么用

### 2.1 录制

```bash
# 环境变量 ION_RECORD=<id> 开启录制，正常用
ION_RECORD=fix-bug ion --model glm-4.6 "修复 calc 函数"

# 多轮对话也录
ION_RECORD=refactor-session ion --model glm-4.6 "重构 utils"
# 同一会话内的多轮都录进同一个 trace

# 录制时不影响正常使用——就是多写个文件
```

**录制时发生的事：**
1. 检测到 `ION_RECORD`，启用 `RecordingProvider`（内部包真实 provider）
2. 每次 LLM 调用：先调真实 provider → 拿到完整 `AssistantMessage` → 序列化追加到 `~/.ion/recordings/<id>/trace.jsonl`
3. 用户正常用，该调工具调工具，该多轮多轮
4. 进程退出，录制已持久化在磁盘

**录制 meta：** 同目录下写 `meta.json`，记录模型、时间、响应数、工具调用数，便于管理。

### 2.2 回放

```bash
# 用录制 ID 当 model 名，provider 是 replay
ion --model replay/fix-bug "修复 calc 函数"

# 回放时不需要 API key、不联网
# agent 的每一轮 LLM 调用都拿录制的响应
# 工具真实执行（read/edit/bash 都是真的）
```

**回放时发生的事：**
1. 解析 `--model replay/fix-bug` → `provider="replay"`, `model_id="fix-bug"`
2. `ReplayProvider`（注册成 `"replay"` ApiProvider）被路由到
3. 用 `model_id` 作为录制 ID，调 `load_script(~/.ion/recordings/fix-bug/trace.jsonl)` 加载响应
4. 内部就是一个 FauxProvider 实例，set_responses 后 FIFO 回放
5. 工具真实执行，但 LLM 响应是录制的——整个会话的决策被复现

### 2.3 管理（可选 CLI）

```bash
ion recordings list                    # 列出所有录制
ion recordings show fix-bug            # 看某次录制的 meta + 摘要
ion recordings delete fix-bug          # 删录制
```

便利性命令，不是核心。第一期可不实现——录制文件就是 JSONL，用户可以直接看。

---

## 3. 架构

### 3.1 RecordingProvider（录制）

**位置：** `ion-provider/src/record.rs`（新文件）

```rust
/// 录制 Provider：包一个真实 provider，把每次响应写到磁盘。
pub struct RecordingProvider {
    /// 被包装的真实 provider（glm-4.6 / deepseek 等）
    inner: Box<dyn ApiProvider>,
    /// 录制文件路径（trace.jsonl）
    trace_path: PathBuf,
    /// meta 文件路径（meta.json）
    meta_path: PathBuf,
    /// 共享状态：调用计数、工具调用统计（meta 更新用）
    state: Arc<Mutex<RecordingState>>,
}

#[async_trait]
impl ApiProvider for RecordingProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        // 1. 调真实 provider
        let inner_stream = self.inner.stream(model, context, options).await?;

        // 2. 包装 EventStream：透传事件 + 收集最终 message
        //    EventStream 内部是 mpsc channel，需要 tap 一份
        let (mut tap_stream, tap_sender) = EventStream::new();
        let trace_path = self.trace_path.clone();
        let state = self.state.clone();

        // spawn 一个任务：读 inner_stream，转发给调用者 + 收集最终 message 写盘
        tokio::spawn(async move {
            let mut final_message = None;
            let mut event_stream = inner_stream;
            while let Some(ev) = event_stream.recv().await {
                tap_sender.push(ev.clone());  // 透传给 agent
                if let StreamEvent::Done { message, .. } = &ev {
                    final_message = Some(message.clone());
                }
            }
            // 写盘
            if let Some(msg) = final_message {
                write_trace_line(&trace_path, &msg);
                update_meta(&state, &msg);
            }
            // tap_sender drop 时 oneshot 完成
        });

        Ok(tap_stream)
    }
}
```

**关键点：透传 + tap。** RecordingProvider 不改变响应内容，只是把 inner provider 的事件流复制一份给调用者，同时把最终 message 写盘。

### 3.2 ReplayProvider（回放）

**位置：** `ion-provider/src/replay.rs`（新文件）

```rust
/// 回放 Provider：根据 model_id（=录制 ID）加载 trace，FIFO 回放。
/// 本质是 FauxProvider + 自动加载逻辑。
pub struct ReplayProvider;

#[async_trait]
impl ApiProvider for ReplayProvider {
    async fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        // model.id 就是录制 ID
        let recording_id = &model.id;
        let trace_path = recordings_dir().join(recording_id).join("trace.jsonl");

        if !trace_path.exists() {
            return Err(ProviderError::Stream(format!(
                "recording '{}' not found at {}", recording_id, trace_path.display()
            )));
        }

        // 复用 load_script 加载（格式和 faux 脚本一致）
        let steps = crate::faux::load_script(&trace_path)?;
        let faux = crate::faux::FauxProvider::new();
        faux.set_responses(steps);

        // 委托给 FauxProvider::stream
        faux.stream(model, _context, _options).await
    }
}
```

**关键复用：** ReplayProvider 几乎是个壳——加载 trace 后委托给 FauxProvider。FauxProvider 已经处理了流式分块、loud failure、StopReason 分支。

### 3.3 注册

**在 ion-provider 的 `register_builtins` 之后**，二进制启动时注册：

```rust
// src/bin/ion_worker.rs 和 src/bin/ion.rs 的 build_registry_and_model
registry.register_builtins();

// Record/Replay provider（总是注册，按需启用）
registry.register("replay", Box::new(ion_provider::replay::ReplayProvider));

// 录制是包装式的，不预注册——检测到 ION_RECORD 时动态包装
```

### 3.4 录制触发（环境变量）

在 `build_registry_and_model`（ion.rs）和 ion_worker.rs 启动逻辑里：

```rust
let recording_id = std::env::var("ION_RECORD").ok();
let model = /* 正常解析的 model */;

let (registry, model) = if let Some(id) = recording_id {
    // 录制模式：用 RecordingProvider 包装真实 provider
    let trace_path = recordings_dir().join(&id).join("trace.jsonl");
    let meta_path = recordings_dir().join(&id).join("meta.json");

    // 防 ID 冲突覆盖（除非显式允许）
    if trace_path.exists() && std::env::var("ION_RECORD_OVERWRITE").is_err() {
        eprintln!("[record] recording '{}' already exists. Set ION_RECORD_OVERWRITE=1 to overwrite.", id);
        std::process::exit(1);
    }
    std::fs::create_dir_all(trace_path.parent().unwrap()).ok();

    // 包装：把原 provider 替换成 RecordingProvider
    let inner = registry.get(&model.api).map(|p| /* clone the provider box */);
    // （注意：Box<dyn ApiProvider> 不能 clone，需要重构 registry 或换注册方式——见 §6 注意点）
    registry.register(
        &model.api,
        Box::new(RecordingProvider::new(inner, trace_path, meta_path)),
    );
    eprintln!("[record] recording to {} (model: {})", trace_path.display(), model.id);
    (registry, model)
} else {
    (registry, model)
};
```

### 3.5 回放触发（`--model replay/<id>`）

不需要特殊代码——现有的 `--model provider/id` 解析已经处理：
- `--model replay/fix-bug` → `provider="replay"`, `model_id="fix-bug"`
- `model.api = "replay"`（因为 replay 注册成 ApiProvider key）
- dispatch 时 `registry.get("replay")` 拿到 ReplayProvider
- ReplayProvider 用 `model.id="fix-bug"` 加载录制

**零侵入：** 复用现有 model 解析 + registry 路由。

---

## 4. 数据格式

### 4.1 trace.jsonl（录制文件）—— faux 脚本超集

每行一条 LLM 响应。**Phase 1 用兼容格式**（和 faux 脚本一致，`load_script` 直接能读），但每行额外带 `request_hash`（评审 #5/#6）：

```jsonl
{"text":"我读文件","tool_call":{"name":"read","input":{"path":"Cargo.toml"}},"request_hash":"a1b2c3d4e5f6a7b8"}
{"text":"用 tokio，我改一下","tool_call":{"name":"edit","input":{"path":"src/main.rs","content":"..."}},"request_hash":"c8d9e0f1a2b3c4d5"}
{"text":"改完了","request_hash":"e6f7a8b9c0d1e2f3"}
```

**向后兼容：** `load_script` 的 `parse_script_line` 忽略未知字段（`request_hash` 是可选的），所以：
- 旧 faux 脚本（无 request_hash）仍能加载
- 录制的 trace（带 request_hash）也能被 `load_script` 加载，hash 字段被忽略（FauxProvider 不用它）

**Phase 2 可平滑升级到完整超集格式**（带 schema_version）：
```jsonl
{"type":"assistant_response","schema_version":1,"step":1,"request_hash":"...","model":"glm-4.6","response":{...},"usage":{...}}
```
Phase 1 不做完整超集，但 meta.json 从一开始就带 `schema_version: 1`（零成本，避免未来迁移痛）。

支持的行格式（对齐 `parse_script_line`，request_hash 可选）：
- `{"text":"..."}` — 纯文本
- `{"tool_call":{"name":"...","input":{...}}}` — 工具调用
- `{"thinking":"...","text":"..."}` — 思考 + 文本
- `{"text":"","stop_reason":"error","error_message":"..."}` — 错误

### 4.2 meta.json（元信息）

```json
{
  "schema_version": 1,
  "id": "fix-bug-2026-07-08",
  "model": "glm-4.6",
  "provider": "zhipuai",
  "created_at": 1720416000000,
  "response_count": 3,
  "tool_call_count": 2,
  "tool_calls": [
    {"name": "read", "input_summary": "{\"path\":\"Cargo.toml\"}"},
    {"name": "edit", "input_summary": "{\"path\":\"src/main.rs\"}"}
  ],
  "duration_ms": 45000
}
```

meta 更新用 tmp + rename（防崩溃半截 JSON）：
```rust
std::fs::write(&meta_tmp, content)?;
std::fs::rename(&meta_tmp, &meta_path)?;  // 原子
```

### 4.3 目录结构

```
~/.ion/recordings/                    0700 权限（含源码/命令/可能含 secret）
├── fix-bug-2026-07-08/
│   ├── trace.jsonl                   0600 权限
│   ├── meta.json                     0600 权限
│   └── .lock                         录制时独占（评审 #11）
├── refactor-session/
│   ├── trace.jsonl
│   ├── meta.json
│   └── .lock
└── ...
```

---

## 5. 关键设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| **回放匹配策略** | FIFO（按录制顺序）+ Phase 1 记录 request_hash（不强制） | 简单；pi 和 VCR 库默认；hash 记录为 Phase 2 strict 校验铺路 |
| **回放时工具执行** | 真实执行（默认），但**必须经 SecuredRuntime** | 工具副作用是真的；权限系统不能绕过；见 §5.1 安全保护 |
| **工具结果录制** | 不录（Phase 1） | 副作用难完美录（bash 输出、时间相关）；Phase 2 可选 |
| **存储格式** | JSONL（faux 脚本超集） | 复用 `load_script`；meta 加 `schema_version` 便于未来扩展 |
| **录制 ID 冲突** | 报错防覆盖 | 默认安全；`ION_RECORD_OVERWRITE=1` 显式允许覆盖 |
| **录制 ID 安全校验** | 正则 `^[a-zA-Z0-9._-]{1,80}$` + canonicalize 防穿越 | 防 `replay/../../etc/passwd` 路径穿越（见 §6.3） |
| **并发录制** | `.lock` 文件独占 | 防同 ID 多进程写坏 trace（见 §6.4） |
| **触发方式** | 环境变量（录）/ `--model`（放） | 环境变量不需 CLI 开关；`--model` 复用现有语法 |
| **持久化时机** | 每条响应立即 flush trace；meta 用 tmp+rename | 崩溃保留已录；meta 不半截 |
| **录制时是否记 input** | 不记完整 context（太重）；记 request_hash | meta 记工具调用摘要；hash 用于回放轨道校验 |
| **Provider 包装方式** | ProviderFactory（不是 boxed_clone） | boxed_clone 让 provider 内部状态语义混乱（见 §6.2） |

### 5.1 回放时的安全保护（评审反馈，Phase 1 必做）

回放时工具真实执行，有副作用风险。三条零成本保护：

1. **醒目提示**：回放启动时打印
   ```
   [replay] ⚠️  Tools will execute for real. Recording decisions from 'fix-bug'.
   [replay] ⚠️  Ensure you are in an isolated workspace (not your real project).
   ```

2. **必须经 SecuredRuntime**：回放走完整 agent loop，工具执行仍经过 `PermissionEngine` + `CommandGuard`。**Provider replay 不能伪造权限，不能绕过权限系统。** 这是已有行为（agent loop 设计如此），文档明确声明。

3. **CI/测试推荐临时目录**：回放应在 `mktemp -d` 或 worktree 隔离目录里跑，不在真实项目里。验收用例和测试说明都要写明这点。

**`ION_REPLAY_EXECUTION` 四模式（live/sandbox/mock/confirm）放 Phase 2**——成本高，Phase 1 用上面三条零成本保护足够。

### 5.2 核心原则（评审反馈 #14）

```
Provider replay 只能伪造 LLM 响应；
不能伪造权限；
不能绕过 SecuredRuntime；
不能默认信任 trace 里的工具调用。
```

---

## 6. 实现注意点

### 6.1 EventStream tap —— 沉淀成公共工具（评审 #7）

`EventStream` 内部是 `mpsc::Receiver<StreamEvent>`，单消费者。RecordingProvider 要"转发 + tap 最终 message"，必须正确处理 `sender.end()` 的 oneshot。

**不要把这段逻辑只写在 RecordingProvider 里，沉淀成公共工具**（评审建议）——因为 debug trace / stream logging / token usage capture / observability 都能用：

```rust
// ion-provider/src/event_stream.rs 加这个方法
impl EventStream {
    /// 转发 inner stream 的事件，同时 tap 出最终的 Done/Error message。
    /// on_done 在收到 Done 时同步调用（用于录制）。
    /// 正确处理 sender.end() 的 oneshot 完成。
    pub fn forward_with_done_tap<F>(
        mut inner: EventStream,
        on_done: F,
    ) -> EventStream
    where
        F: FnOnce(&AssistantMessage) + Send + 'static,
    {
        let (tap_stream, tap_sender) = EventStream::new();
        tokio::spawn(async move {
            let mut final_msg: Option<AssistantMessage> = None;
            let mut final_reason: Option<StopReason> = None;
            while let Some(ev) = inner.recv().await {
                match &ev {
                    StreamEvent::Done { message, reason } => {
                        final_msg = Some(message.clone());
                        final_reason = Some(reason.clone());
                    }
                    StreamEvent::Error { message, reason } => {
                        final_msg = Some(message.clone());
                        final_reason = Some(reason.clone());
                    }
                    _ => {}
                }
                tap_sender.push(ev);
            }
            // inner 已结束，必须完成 tap 的 oneshot（不能简单 drop tap_sender）
            match (final_msg, final_reason) {
                (Some(msg), Some(StopReason::Error | StopReason::Aborted)) => {
                    tap_sender.error(msg.stop_reason.clone(), msg);
                }
                (Some(msg), _) => {
                    tap_sender.end(msg);
                }
                _ => {
                    // inner 异常结束无 Done/Error —— 触发 stream 错误
                    // tap_sender drop 会让 result() 报 "stream ended without result"
                }
            }
        });
        tap_stream
    }
}
```

RecordingProvider 的 stream() 就变得极简：
```rust
let inner_stream = self.inner.stream(model, context, options).await?;
let trace_path = self.trace_path.clone();
Ok(EventStream::forward_with_done_tap(inner_stream, move |msg| {
    write_trace_line(&trace_path, msg);
}))
```

### 6.2 ProviderFactory —— 代替 boxed_clone（评审 #8）

`Box<dyn ApiProvider>` 不能 clone。录制要"包装原 provider"。

**不用 boxed_clone**（让 provider 内部状态如 client/cache/连接池语义混乱），用 ProviderFactory：

```rust
// ion-provider/src/registry.rs 加
pub trait ProviderFactory: Send + Sync {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>>;
}

// 把现有 register_builtins 重构成基于 factory
pub struct BuiltinProviderFactory;
impl ProviderFactory for BuiltinProviderFactory {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>> {
        match api {
            "openai-completions" => Some(Box::new(OpenAICompletionsProvider)),
            "anthropic-messages" => Some(Box::new(AnthropicMessagesProvider)),
            "openai-responses" => Some(Box::new(OpenAIResponsesProvider)),
            "google-generative-ai" => Some(Box::new(GoogleGenerativeAIProvider)),
            _ => None,
        }
    }
}
```

录制时的包装流程：
```rust
let factory = BuiltinProviderFactory;
let real_api = model.api.clone();  // "openai-completions"
if let Some(real) = factory.create(&real_api) {
    let recording = RecordingProvider::new(real, trace_path, meta_path);
    registry.register(&real_api, Box::new(recording));
}
```

`register_builtins` 保留（向后兼容），factory 是录制专用的构造途径。

### 6.3 录制 ID 路径穿越校验（评审 #9，安全 P0）

`ion --model replay/<id>` 的 `<id>` 直接拼路径。必须防：

```rust
// ion-provider/src/replay.rs
fn validate_recording_id(id: &str) -> ProviderResult<()> {
    // 只允许安全字符
    let re = regex::Regex::new(r"^[a-zA-Z0-9._-]{1,80}$").unwrap();
    if !re.is_match(id) {
        return Err(ProviderError::Stream(format!(
            "invalid recording id '{}': only [a-zA-Z0-9._-] allowed, max 80 chars", id
        )));
    }
    Ok(())
}

fn recording_path(id: &str) -> ProviderResult<PathBuf> {
    validate_recording_id(id)?;
    let base = recordings_dir();
    let path = base.join(id).join("trace.jsonl");
    // canonicalize 校验：最终路径必须在 recordings_dir 下
    let canonical = path.canonicalize().unwrap_or(path.clone());
    if !canonical.starts_with(&base) {
        return Err(ProviderError::Stream(format!(
            "recording id escapes recordings dir: {}", id
        )));
    }
    Ok(path)
}
```

录制时（`ION_RECORD=<id>`）同样校验。

**文件权限（因为 trace 含源码/命令/可能含 secret）：**
```rust
// 创建 recordings 目录时
std::fs::set_permissions(&recordings_dir(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).ok();
// 写 trace.jsonl / meta.json 时
std::fs::set_permissions(&trace_path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).ok();
```

### 6.4 并发录制 lock（评审 #11）

同 ID 多进程录制会写坏 trace。加文件锁：

```rust
// 录制开始时
let lock_path = recordings_dir().join(id).join(".lock");
let lock = std::fs::OpenOptions::new().create_new(true).write(true).open(&lock_path);
match lock {
    Ok(mut f) => {
        use std::io::Write;
        let _ = writeln!(f, "{}", std::process::id());
        // lock 持有到进程退出（文件保留，下次 OVERWRITE 时清理）
    }
    Err(_) if trace_path.exists() => {
        // 已有录制 + lock 占用
        if std::env::var("ION_RECORD_OVERWRITE").is_err() {
            return Err(format!("recording '{}' already exists or is active. Set ION_RECORD_OVERWRITE=1 to overwrite.", id));
        }
        // OVERWRITE：删 lock 重新拿
        let _ = std::fs::remove_file(&lock_path);
        // ... 重新创建 lock + 清空 trace
    }
    Err(e) => return Err(format!("failed to acquire recording lock: {}", e)),
}
```

### 6.5 request_hash 记录（评审 #5，Phase 1 记录不强制）

Phase 1 录制时记录 hash，回放时**只警告不强制**。Phase 2 加 strict 模式。

录制时算 hash：
```rust
fn request_hash(context: &Context, model: &Model) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    // hash 关键输入：messages + tools + model + system_prompt
    context.system_prompt.hash(&mut h);
    context.messages.len().hash(&mut h);  // 用 len 而非全文，避免 trace 过大
    for msg in &context.messages {
        msg.role.hash(&mut h);
    }
    model.id.hash(&mut h);
    format!("{:016x}", h.finish())
}
```

trace 每行加 `request_hash` 字段（超集格式，见 §4.1）。

回放时：
```rust
// ReplayProvider 里，每次 stream 调用
let current_hash = request_hash(context, model);
let recorded_hash = &recorded_step.request_hash;
if current_hash != *recorded_hash {
    eprintln!("[replay] ⚠️  request hash mismatch at step {}: recorded {} but now {} (path may have diverged)", step, recorded_hash, current_hash);
    // Phase 1: 只警告，继续 FIFO
    // Phase 2: ION_REPLAY_STRICT=1 时报错
}
```

### 6.6 流式录制 vs 非流式录制

录制只关心**最终的 `AssistantMessage`**（Done 事件携带的），不录每个 delta。理由：
- delta 是流式 UI 用的，回放时 ReplayProvider 会自己重新切 delta（FauxProvider 已做）
- 只录最终 message 大幅减小录制文件体积

所以 RecordingProvider 只在收到 `Done` 事件时写一行 trace（通过 `forward_with_done_tap` 的 `on_done` 回调）。

---

## 7. 验收用例

### P0（必须通过 — 安全 + 核心功能）

| # | 用例 | 操作 | 预期 |
|---|------|------|------|
| RR-P0.1 | 基本录制 | `ION_RECORD=t1 ion --model glm-4.6 "hi"` | `~/.ion/recordings/t1/trace.jsonl` 存在，至少 1 行 |
| RR-P0.2 | 录制含工具调用 | 录制一个调 read 工具的会话 | trace 里有 `tool_call` 行 |
| RR-P0.3 | 基本回放 | `ion --model replay/t1 "hi"` | 不联网，输出和录制时的响应一致 |
| RR-P0.4 | 回放多轮 | 录制 3 轮会话，回放 | 3 轮响应依次复现，工具真实执行 |
| RR-P0.5 | 回放免 API key | 不设任何 key 环境变量回放 | 正常工作 |
| RR-P0.6 | 录制 ID 冲突报错 | 同 ID 二次录制（无 OVERWRITE） | 报错退出，不覆盖 |
| **RR-P0.7** | **路径穿越拒绝**（评审 #9） | `ion --model replay/../../etc/passwd` | 报错 "invalid recording id"，不读任意文件 |
| **RR-P0.8** | **录制 ID 非法字符拒绝** | `ion --model replay/a b/c` 或含空格/特殊字符 | 报错 |
| **RR-P0.9** | **回放经 SecuredRuntime**（评审 #4/#14） | 录制含 `bash rm`，回放时 CommandGuard 拦截危险命令 | 权限系统照常工作，不因 replay 绕过 |
| **RR-P0.10** | **回放醒目提示**（评审 #4） | 启动回放 | stderr 打印 "Tools will execute for real" 警告 |
| **RR-P0.11** | **trace exhausted loud failure** | 录 2 条，回放时 agent 要 3 轮 | 第 3 轮报错 "no more recorded responses" |
| **RR-P0.12** | **文件权限**（评审 #9） | 录制后查权限 | recordings 目录 0700，trace/meta 0600 |

### P1（应该通过 — 健壮性 + 管理）

| # | 用例 | 预期 |
|---|------|------|
| RR-P1.1 | 录制 ID 覆盖 | `ION_RECORD_OVERWRITE=1` 二次录制 | 覆盖成功 |
| RR-P1.2 | 回放不存在的 ID | `--model replay/nonexistent` | 报错 "recording not found" |
| RR-P1.3 | meta.json 正确（含 schema_version） | 录制后 meta 字段完整 | schema_version=1，response_count 准确 |
| RR-P1.4 | 三场景都支持录制 | 场景 1/2/3 都能 `ION_RECORD=...` 录 | host 子进程也继承录制 |
| RR-P1.5 | 回放时工具真实执行 | 录制调了 edit，回放时文件真被改 | 工具副作用发生 |
| **RR-P1.6** | **request_hash 记录**（评审 #5） | 录制后 trace 每行有 request_hash | 字段存在 |
| **RR-P1.7** | **request_hash 不匹配警告**（Phase 1 不强制） | 回放时 context 变了 | stderr 警告，继续回放 |
| **RR-P1.8** | **并发录制 lock**（评审 #11） | 两进程同 ID 录制 | 第二个报错 "already active" |
| **RR-P1.9** | **`ion recordings list`**（评审 #10） | 列出所有录制 | 显示 id/model/时间/响应数 |
| **RR-P1.10** | **trace leftover 检测** | 录 3 条，回放只消耗 2 轮 | 进程退出时警告 "2 recorded responses unused" |

### XFail（预期失败 — 已知限制）

| # | 用例 | 原因 | 何时修复 |
|---|------|------|---------|
| RR-X1 | 回放时 agent 走了不同路径，request_hash strict 模式拒绝 | Phase 1 只警告不强制 | Phase 2 加 `ION_REPLAY_STRICT=1` |
| RR-X2 | 录制了 bash 工具，回放时命令输出不同 | 工具副作用不录 | Phase 2 可选录工具结果 |
| RR-X3 | 跨模型回放（用 A 录，想看 B 的行为） | 录制绑定的是 A 的决策，不是模型行为本身 | 设计上不支持（见 §10 边界） |
| RR-X4 | `ION_REPLAY_EXECUTION=mock`（不执行工具） | Phase 2 功能 | Phase 2 |

---

## 8. 实现顺序（按评审 #15 重排）

### Phase 1：录制 + 回放 + 安全（评审重排后的 P0）

| Step | 内容 | 评审意见 | 预估 |
|------|------|---------|------|
| 1 | `EventStream::forward_with_done_tap` 公共工具 | #7 | 0.5 天 |
| 2 | `ProviderFactory` trait + BuiltinProviderFactory | #8 | 0.5 天 |
| 3 | `RecordingProvider`（用 forward_with_done_tap + 写 trace/meta） | — | 0.5 天 |
| 4 | `ReplayProvider`（壳，load trace + FauxProvider） | — | 0.5 天 |
| 5 | 录制 ID 校验（正则 + canonicalize） + 文件权限 | #9 | 0.5 天 |
| 6 | 并发 lock + 冲突报错 + OVERWRITE | #11 | 0.5 天 |
| 7 | request_hash 记录（Phase 1 不强制） | #5 | 0.5 天 |
| 8 | 回放安全提示 + 经 SecuredRuntime 验证 | #4 | 0.5 天 |
| 9 | 三场景接入（ion_worker + 环境变量传递） | — | 0.5 天 |
| 10 | `ion recordings list` 命令 | #10 | 0.5 天 |
| 11 | 验收用例（P0×12 + P1×10） | — | 1 天 |
| **合计** | | | **~6 天** |

### Phase 2：strict 校验 + 完整管理（后续）

| Step | 内容 | 预估 |
|------|------|------|
| 1 | `ION_REPLAY_STRICT=1`（request_hash 不匹配报错） | 0.5 天 |
| 2 | `ION_REPLAY_EXECUTION=sandbox/mock/confirm` 四模式 | 2 天 |
| 3 | 工具结果录制（`ION_REPLAY_MOCK_TOOLS=1`） | 1 天 |
| 4 | `ion recordings show/delete` + recording scrub | 1 天 |
| 5 | trace 完整超集格式（schema_version=1 全字段） | 0.5 天 |
| 6 | replay diff（两次回放对比） | 1 天 |
| 7 | 录制导入导出（跨机器复现） | 1 天 |
| **合计** | | **~7 天** |

---

## 9. 在测试体系里的位置

Record/Replay 完善了 ION 的测试金字塔：

```
                    ┌─────────────────────┐
                    │ 真实 LLM E2E        │  ← 真实 API，慢，#[ignore]
                    │ (cli_e2e_real.rs)   │
                    └─────────────────────┘
                  ┌─┴───────────────────┴─┐
                  │ Record/Replay 回放     │  ← 复现真实会话，免 key，CI 可跑
                  │ (本文档)              │
                  └─┬───────────────────┬─┘
                ┌───┴───────────────────┴───┐
                │ FauxProvider 手写脚本     │  ← 构造特定测试场景
                │ (faux_test.rs)            │
                └───┬───────────────────┬───┘
              ┌─────┴───────────────────┴─────┐
              │ 单元测试（纯数据层）           │  ← 快，无 LLM
              │ (session_jsonl, memory 等)    │
              └───────────────────────────────┘
```

| 层级 | 速度 | 真实度 | 用途 |
|------|------|--------|------|
| 单元测试 | 毫秒 | 数据层 | 验证数据结构 |
| FauxProvider | 毫秒 | agent 链路 + 手写响应 | 构造"如果 LLM 这么回"的场景 |
| **Record/Replay** | 毫秒 | **agent 链路 + 真实决策复现** | **回归测试、复现 bug** |
| 真实 LLM | 秒 | 全真实 | 烟测、最终验证 |

**Record/Replay 填补了"FauxProvider 太人工"和"真实 LLM 太慢"之间的空缺**——用真实会话的决策，但免 key 免网络。

---

## 10. 与 FauxProvider 的对比使用指南

| 我想... | 用 FauxProvider | 用 Record/Replay |
|---------|----------------|-----------------|
| 测试"如果 LLM 回 X，agent 会怎样" | ✅ 手写脚本 | ❌ 太重 |
| 回归测试某个真实修复 | ❌ 手写太累 | ✅ 录一次，反复放 |
| 复现一个 bug | ❌ 不知道 LLM 当时回了啥 | ✅ 录当时的会话，回放 |
| CI 跑 agent 行为 | ✅ 可以 | ✅ 更真实 |
| **验证 Agent 在固定旧模型决策下是否稳定** | ❌ | ✅ 用 Record/Replay（决策固定，测 Agent 实现） |
| **评价新模型智能水平** | ❌ | ❌ **不能用 replay**——录制的是旧模型 A 的决策，回放复现的是 A 的行为，不是新模型 B 的行为。评价新模型要重新跑真实模型，再和旧 recording 做 diff |
| Session Tree 验收 | ✅ 构造分支场景 | 可录一个真实分叉会话做回归 |

### 10.1 边界澄清（评审 #13）

Record/Replay 能做的：
- ✅ 比较 **Agent 行为回归**——同一份录制，不同版本的 agent 实现跑，看 agent 是否还按预期处理那些决策
- ✅ 复现 **特定模型在某任务上的决策序列**——固化为 CI 回归

Record/Replay **不能**做的：
- ❌ 评价新模型智能水平（回放的是录制的决策，新模型没参与）
- ❌ 完整环境复现（工具副作用是真的，不是录的）

**想对比新旧模型？** 两个模型各跑一次真实会话（不回放），各自录制，然后 diff 两个 recording——看决策差异。这是 Phase 2 的 "replay diff" 功能。

---

## 11. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | Phase 2：请求 hash 匹配（处理路径分叉） | P2 |
| 2 | Phase 2：工具结果录制（`ION_REPLAY_MOCK_TOOLS`） | P2 |
| 3 | `ion recordings list/show/delete` 管理命令 | P2 |
| 4 | 录制编辑工具（裁剪敏感信息、合并多次录制） | P3 |
| 5 | 录制回放 diff（两次回放的差异对比） | P3 |
| 6 | 录制分享格式（导出/导入，跨机器复现） | P3 |
