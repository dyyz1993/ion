# Record/Replay 设计文档

> **状态：设计稿** — 录制/回放系统，环境变量驱动录制、`--model replay/<id>` 回放。复用已实现的 FauxProvider 作为回放引擎。让"复现某个真实会话"变成一行命令。

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

### 4.1 trace.jsonl（录制文件）

每行一条 LLM 响应，格式**和 faux 脚本一致**（复用 `load_script` 解析）：

```jsonl
{"text":"我读文件","tool_call":{"name":"read","input":{"path":"Cargo.toml"}}}
{"text":"用 tokio，我改一下","tool_call":{"name":"edit","input":{"path":"src/main.rs","content":"..."}}}
{"text":"改完了"}
```

支持的行格式（对齐 `parse_script_line`）：
- `{"text":"..."}` — 纯文本
- `{"tool_call":{"name":"...","input":{...}}}` — 工具调用
- `{"thinking":"...","text":"..."}` — 思考 + 文本
- `{"text":"","stop_reason":"error","error_message":"..."}` — 错误

### 4.2 meta.json（元信息）

```json
{
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

### 4.3 目录结构

```
~/.ion/recordings/
├── fix-bug-2026-07-08/
│   ├── trace.jsonl      # LLM 响应序列
│   └── meta.json        # 元信息
├── refactor-session/
│   ├── trace.jsonl
│   └── meta.json
└── ...
```

---

## 5. 关键设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| **回放匹配策略** | FIFO（按录制顺序） | 简单；pi 和 VCR 库默认；路径分叉时报错暴露问题 |
| **回放时工具执行** | 真实执行（默认） | 工具副作用是真实的；LLM 决策复现；大部分场景够用 |
| **工具结果录制** | 不录（Phase 1） | 副作用难完美录（bash 输出、时间相关）；Phase 2 可选 |
| **存储格式** | JSONL（对齐 faux 脚本） | 复用 `load_script`；和 session.jsonl 风格一致 |
| **录制 ID 冲突** | 报错防覆盖 | 默认安全；`ION_RECORD_OVERWRITE=1` 显式允许覆盖 |
| **触发方式** | 环境变量（录）/ `--model`（放） | 环境变量不需 CLI 开关；`--model` 复用现有语法 |
| **持久化时机** | 每条响应立即 flush | 进程崩溃也能保留已录制的部分 |
| **录制时是否记 input** | 不记完整 context（太重） | meta 记工具调用摘要便于查阅；完整 context 后续可选 |

---

## 6. 实现注意点

### 6.1 `Box<dyn ApiProvider>` 不能 clone 的问题

录制要"包装原 provider"，但 `ApiRegistry::get` 返回 `&dyn ApiProvider`，没法拿出 owned 的 box 来包装。

**解决方案（三选一）：**

| 方案 | 做法 | 优缺点 |
|------|------|--------|
| **A. 重注册** | `register_builtins` 后立刻在录制模式下用 `RecordingProvider` 包，而不是事后替换 | 最干净；但要知道 model 用哪个 api 才能包对 |
| **B. registry 加 clone** | 给 ApiRegistry 加 `clone_provider(api) -> Option<Box<dyn ApiProvider>>`，每个 provider 加 `fn boxed_clone(&self) -> Box<dyn ApiProvider>` | 通用；但要给所有 provider 实现 boxed_clone |
| **C. 不包装，改 stream 拦截** | 在 agent_loop.rs 的 `registry::stream` 调用点之后拦截 EventStream，录制不发生在 provider 层 | 不改 registry；但要在多个调用点都加 |

**建议方案 A**：在 `build_registry_and_model` 里，如果检测到 `ION_RECORD`，根据 model 的 api 决定包哪个真实 provider，直接重注册：

```rust
if let Some(id) = std::env::var("ION_RECORD").ok() {
    let real_api = model.api.clone();  // 比如 "openai-completions"
    // 重新构造：先注册真实 provider，再用 Recording 包装重注册到同 key
    let real = build_real_provider(&real_api);  // 工厂函数
    registry.register(&real_api, Box::new(
        RecordingProvider::new(real, trace_path, meta_path)
    ));
}
```

需要给每个真实 provider 加个工厂函数（或把 `register_builtins` 重构成 `build_provider(api) -> Box<dyn ApiProvider>`）。

### 6.2 EventStream tap 的正确性

`EventStream` 内部是 `mpsc::Receiver<StreamEvent>`。RecordingProvider 拿到 inner 的 EventStream 后要"复制一份"，但 mpsc 是单消费者。

**解决：** 起一个转发任务，从 inner stream 读事件，每条 push 到新创建的 tap sender：

```rust
let (tap_stream, tap_sender) = EventStream::new();
tokio::spawn(async move {
    let mut inner = inner_stream;
    let mut final_msg = None;
    while let Some(ev) = inner.recv().await {
        if matches!(ev, StreamEvent::Done{..} | StreamEvent::Error{..}) {
            if let StreamEvent::Done{message,..} = &ev { final_msg = Some(message.clone()); }
        }
        tap_sender.push(ev);  // EventSender::push 是 &self，可循环调用
    }
    // tap_sender drop → tap channel 关闭 → 调用者的 recv 返回 None
    // 但要先完成 oneshot！
    // —— 这里复杂：EventStream::result() 等 oneshot，tap_sender drop 不触发 oneshot
    // 解决：手动管理 result_tx
});
```

**这块是实现的难点。** `EventStream::result()` 依赖 `sender.end(message)` 消费 sender 并触发 oneshot。RecordingProvider 的转发任务必须在 inner 完成后，对 tap_sender 调 `.end(final_msg)`，而不是简单 drop。

可能需要给 EventStream 加一个 `forward_from(inner) -> EventStream` 工具方法，封装这个"转发 + tap 最终 message"的逻辑。

### 6.3 流式录制 vs 非流式录制

录制只关心**最终的 `AssistantMessage`**（Done 事件携带的），不录每个 delta。理由：
- delta 是流式 UI 用的，回放时 ReplayProvider 会自己重新切 delta（FauxProvider 已做）
- 只录最终 message 大幅减小录制文件体积

所以 RecordingProvider 只在收到 `Done` 事件时写一行 trace。

---

## 7. 验收用例

### P0（必须通过）

| # | 用例 | 操作 | 预期 |
|---|------|------|------|
| RR-P0.1 | 基本录制 | `ION_RECORD=t1 ion --model glm-4.6 "hi"` | `~/.ion/recordings/t1/trace.jsonl` 存在，至少 1 行 |
| RR-P0.2 | 录制含工具调用 | 录制一个调 read 工具的会话 | trace 里有 `tool_call` 行 |
| RR-P0.3 | 基本回放 | `ion --model replay/t1 "hi"` | 不联网，输出和录制时的响应一致 |
| RR-P0.4 | 回放多轮 | 录制 3 轮会话，回放 | 3 轮响应依次复现，工具真实执行 |
| RR-P0.5 | 回放免 API key | 不设任何 key 环境变量回放 | 正常工作 |
| RR-P0.6 | 录制 ID 冲突报错 | 同 ID 二次录制（无 OVERWRITE） | 报错退出，不覆盖 |

### P1（应该通过）

| # | 用例 | 预期 |
|---|------|------|
| RR-P1.1 | 录制 ID 覆盖 | `ION_RECORD_OVERWRITE=1` 二次录制 | 覆盖成功 |
| RR-P1.2 | 回放不存在的 ID | `--model replay/nonexistent` | 报错 "recording not found" |
| RR-P1.3 | meta.json 正确 | 录制后 meta 含 model/response_count/tool_calls | 字段完整 |
| RR-P1.4 | 三场景都支持录制 | 场景 1/2/3 都能 `ION_RECORD=...` 录 | host 子进程也继承录制 |
| RR-P1.5 | 回放时工具真实执行 | 录制调了 edit，回放时文件真被改 | 工具副作用发生 |

### XFail（预期失败 — 已知限制）

| # | 用例 | 原因 | 何时修复 |
|---|------|------|---------|
| RR-X1 | 回放时 agent 走了不同路径（工具结果变了） | FIFO 错位，后续响应对不上当前 context | Phase 2 加请求 hash 匹配 |
| RR-X2 | 录制了 bash 工具，回放时命令输出不同 | 工具副作用不录 | Phase 2 可选录工具结果 |
| RR-X3 | 跨模型回放（用 A 录，想用 B 的行为回放） | 录制绑定的是 A 的决策 | 设计上不支持（回放的是"决策"不是"模型"） |

---

## 8. 实现顺序

### Phase 1：录制 + FIFO 回放（最小可用）

| Step | 内容 | 预估 |
|------|------|------|
| 1 | `RecordingProvider`（EventStream tap + 写盘） | 1 天 |
| 2 | `ReplayProvider`（壳，复用 load_script + FauxProvider） | 0.5 天 |
| 3 | 注册 + 触发逻辑（环境变量 + `--model replay/id`） | 0.5 天 |
| 4 | `recordings_dir()` 路径 + meta.json | 0.5 天 |
| 5 | 三场景接入（ion_worker + 环境变量传递） | 0.5 天 |
| 6 | 验收用例（P0 + P1） | 1 天 |
| **合计** | | **~4 天** |

### Phase 2：健壮性 + 管理（后续）

| Step | 内容 | 预估 |
|------|------|------|
| 1 | 请求 hash 匹配（可选 FIFO 替代） | 1.5 天 |
| 2 | 工具结果录制（`ION_REPLAY_MOCK_TOOLS=1`） | 1 天 |
| 3 | `ion recordings list/show/delete` 管理命令 | 0.5 天 |
| 4 | 录制编辑工具（裁剪/合并录制） | 1 天 |
| **合计** | | **~4 天** |

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
| 验证新模型在同一任务上的表现 | — | 录旧模型 → 切新模型跑（对比） |
| Session Tree 验收 | ✅ 构造分支场景 | 可录一个真实分叉会话做回归 |

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
