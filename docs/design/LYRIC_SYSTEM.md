# LYRIC_SYSTEM.md — 歌词改编系统

> **状态：Phase 1 已实现（网页版 + agent + 网关，零 ion 源码改动）。**
> Phase 2（内置押韵 extension）已规划，待走 A→B 铁律实现。

## 1. 目标

做一个网页版的歌词改编工坊：
- 输入原歌词 + 主题 → 改编师逐句改写并自检押韵（中文十三辙）/音节
- 实时流式展示生成过程
- 审查员终审，给出 APPROVE / REQUEST_CHANGES
- 押韵/音节违规在前端高亮

## 2. 架构

```
浏览器 (webui/index.html)
   │  HTTP POST /rpc  +  WebSocket /ws
   ▼
Node 网关 (webui/gateway.mjs)        ← 独立进程，零 ion 改动
   │  Unix socket JSONL (~/.ion/host.sock)
   ▼
ion serve (host 引擎)
   │  create_session / prompt / subscribe
   ▼
lyricist + critic agent (examples/agents/*.md，零代码)
```

### 为什么有网关层？

ion serve 只暴露 Unix socket，浏览器（JS）无法直连 Unix socket。网关做纯字节透传：
- `POST /rpc` → 转发一行 JSON 到 socket，读回带 `id` 的响应
- `WS /ws?session=X` → 连 socket 发 `{method:"subscribe",session:X}`，逐行透传给浏览器

网关不理解业务逻辑，只做 Unix socket ↔ HTTP/WS 协议转换。

### 为什么不改 ion 内部接 axum？

axum/tower-http 已在 `Cargo.toml` 声明（当前是死代码），`--port` flag 也已存在但被忽略。理论上可以在 `cmd_serve_start`（`src/bin/ion.rs:4178`）加一个 axum spawn 实现开箱即用。但按项目 **A→B 铁律**，任何 `src/` 改动都必须走 `ion --host --agent coordinator` 让 developer 在隔离环境改 + 自跑 CI。Phase 1 优先「快速可用」，所以用外部网关绕开铁律，零 ion 源码改动即可跑通。

## 3. Phase 1 组件（已实现）

### 3.1 Agent 定义（零代码）

| 文件 | 角色 | 工具 |
|------|------|------|
| `examples/agents/lyricist.md` | 改编师 | read, write |
| `examples/agents/critic.md` | 审查员（不改词） | read |

**lyricist 的核心约束**：
- 工作流：读原词 → 分析结构 → 按主题改编 → 自检 → 输出
- 押韵：中文十三辙（发花/梭波/乹斜/一七/姑苏/怀来/灰堆/遥条/由求/言前/人辰/江阳/中东），段内句尾同辙
- 音节：每行与原文偏差 ≤ 1
- 输出：`<lyric_result>` XML（含逐句对照 + rhyme_check + syllable_check + notes）
- 副作用：write 到 `lyrics_output.md` 供 critic 读取

**critic 的核心约束**：
- 只审查不改（disallowed: write/edit/bash）
- 五项 checklist：押韵十三辙、音节、主题切合、可唱性、结构
- 输出：`VERDICT: APPROVE` 或 `VERDICT: REQUEST_CHANGES: <原因>`

### 3.2 网关（`webui/gateway.mjs`，~240 行）

依赖：`ws`（WebSocket server）。其余用 Node 内置（http/net/fs）。

路由：
- `GET /` → `webui/index.html`（静态文件，防路径逃逸）
- `GET /healthz` → `{ok, sock}`
- `POST /rpc` → 转发 JSONL，30s 超时，找带 `id` 的响应行
- `WS /ws?session=&extension=&ui=` → 订阅 host 事件流，逐行透传

### 3.3 前端（`webui/index.html`，单文件，原生 JS，v2）

**v2 新增三大功能：可滚动界面 + 历史记录 + 验证循环。**

- **输入区**：原歌词 textarea + 主题 input + 5 个预设 + agent 选择 + **最大迭代轮数**（默认 3，可配 1-10）
- **实时区**：WebSocket 流式渲染 `text_delta`，`tool_call` 显示橙色 chip
- **轮次时间线**（v2，替代原单个结果区）：每轮一个卡片，含轮次号 / critic 结论 / 押韵+音节检查 / 对照表 / 相比上版改了几句 / 「以此为起点重跑」按钮。最新版置顶，旧版可折叠
- **历史记录面板**（v2）：左侧底部，`list_all_sessions` 列出所有 session，改编 session（前缀 🎵）高亮置顶。点开可「填回重跑」或「在原 session 继续对话」
- **验证循环编排**（v2）：critic 不通过时自动把反馈回灌给 lyricist 回改（同一 session），循环到 APPROVE 或达上限。每轮结果都进时间线
- **原始事件流**：折叠面板（调试用）

**bug 修复**（v2）：
- critic 的 VERDICT 检测现在用统一的 `makeHandler(role)` 工厂，lyricist/critic 都走同一套事件处理（原 bug：VERDICT 检测挂错 stream）
- critic 直接吃 lyricist 的 `<lyric_result>` 文本作为 prompt（原 bug：依赖 `lyrics_output.md` 文件跨 session 传上下文，路径不一致空跑）

样式：深色主题，角色色块，零外部依赖。

**滚动**：`main { grid-template-rows: minmax(0,1fr) }` + `.right { min-height:0 }` —— 修复 grid item 默认 `min-height:auto` 被内容撑开导致 `overflow-y:auto` 失效的问题。

### 3.4 启动（`webui/start.sh`）

一键启动：自动起 `ion serve`（若未运行）→ 安装 `ws` 依赖 → 起网关 → 打印 URL。

## 4. 协议细节（参考实现）

ion socket 协议是 JSONL（行分隔 JSON）。三组关键调用：

### 4.1 创建会话
```bash
POST /rpc
{ "method": "create_session", "params": { "agent": "lyricist" } }
→ 200 { "id":"...", "type":"response", "success":true,
        "data": { "session_id":"sess_xxx", "agent":"lyricist", "status":"created" } }
```

### 4.2 发消息（fire-and-forget）
```bash
POST /rpc  (带 session)
{ "method":"prompt", "session":"sess_xxx", "params": { "text":"改编…" } }
→ 200 { "status":"forwarded", "session":"sess_xxx" }
```
注意：prompt 是 fire-and-forget，真正的生成结果通过 subscribe 事件流推送。

### 4.3 订阅事件流
```bash
WS /ws?session=sess_xxx
→ 发送 { "method":"subscribe", "session":"sess_xxx" }
→ 持续推送：
   { "type":"instance_event", "session":"...", "event":{ "type":"agent_start" } }
   { "type":"instance_event", "event":{ "type":"text_delta", "delta":"改" } }
   { "type":"instance_event", "event":{ "type":"tool_call", "tool":"write" } }
   { "type":"instance_event", "event":{ "type":"agent_end" } }
```

### 4.4 列出历史会话（v2 新增）
```bash
POST /rpc  { "method":"list_all_sessions" }
→ { "data": { "sessions":[ {"id","name","firstMessage","model","turnCount","updatedAt",...} ], "totalCount":N } }
```
前端历史面板用它列出所有 session。改编 session 用 `append_session_name` 命名为 `🎵{主题}·{原词首句}`，列表里前缀 `🎵` 的高亮显示。

### 4.5 会话命名（持久化，v2 新增）
```bash
POST /rpc  { "method":"append_session_name", "session":"sid", "params":{"name":"🎵..."} }
→ { "data": {"status":"appended","name":"..."} }
```
**注意用 `append_session_name` 不是 `set_session_name`** —— 后者只回响应不落盘（host 重启即丢），前者写 SessionIndex + JSONL，持久化。每次 run() 成功后调用。

### 4.6 读取会话消息（v2 新增，用于历史还原）
```bash
POST /rpc  { "method":"get_messages", "session":"sid", "params":{"view":"full","limit":200} }
→ { "data": { "messages":[ {"message":{"Assistant":{"content":[{"Text":{"text":"..."}}]}}}, ... ] } }
```
assistant 文本路径：`.messages[].message.Assistant.content[].Text.text`。历史面板点开时用它提取 `<lyric_result>` 还原展示。

## 5. Phase 2 规划：内置押韵 extension（待走 A→B，本次不实现）

Phase 1 的押韵检测靠 LLM 自检（prompt 约束），质量看模型。Phase 2 把押韵/音节检测做成**确定性硬编码规则**，抄 LSP extension 模式。

### 5.1 新增文件

**`src/lyric_extension.rs`** — `LyricExtension` impl `Extension`
- 持有 `Arc<Mutex<LyricReport>>`
- `on_tool_execution_end`：write/edit `.md`/`.txt` 歌词文件后，跑硬编码十三辙押韵检测 + 音节统计（纯计算，同步）
- `on_context`：注入 `<lyric_issues count=N>` XML 给 LLM（抄 `lsp_extension.rs:1061-1140` 的 `format_diagnostics_xml` 模式）
- `on_gate_check`：有未修押韵问题时 `RetryWith("押韵不符十三辙，请修正")` 强制重写（抄 GoalSupervisor 模式）
- `on_extension_rpc`：暴露 `check` / `report` 给 CLI 调试

**`src/agent/tool.rs`** 新增 `LyricCheckTool`
- impl `Tool`，execute 调 provider 做 LLM 语义审查（主题切合/连贯/情感）
- 结果写进共享 `Arc<Mutex<LyricReport>>`

### 5.2 接入点（ion 源码改动，走 A→B）

| 文件 | 改动 |
|------|------|
| `src/lib.rs` | 加 `pub mod lyric_extension;` |
| `src/bin/ion.rs` `build_tools()` (~L1066) | `tools.register(Box::new(LyricCheckTool::new(shared.clone())))` |
| `src/bin/ion.rs` extension 注册 (~L2083) | `ext_reg.register(Box::new(LyricExtension::new(shared)))` |

### 5.3 十三辙押韵表（硬编码）

```rust
fn rhyme_group(final: &str) -> Option<&'static str> {
    match final {
        "a" | "ia" | "ua" => Some("发花辙"),
        "o" | "e" | "uo" => Some("梭波辙"),
        "ie" | "üe" => Some("乹斜辙"),
        "i" | "ü" | "er" => Some("一七辙"),
        "u" => Some("姑苏辙"),
        "ai" | "uai" => Some("怀来辙"),
        "ei" | "ui" | "uei" => Some("灰堆辙"),
        "ao" | "iao" => Some("遥条辙"),
        "ou" | "iu" | "iou" => Some("由求辙"),
        "an" | "ian" | "uan" | "üan" => Some("言前辙"),
        "en" | "in" | "un" | "ün" => Some("人辰辙"),
        "ang" | "iang" | "uang" => Some("江阳辙"),
        "eng" | "ing" | "ueng" | "ong" | "iong" => Some("中东辙"),
        _ => None,
    }
}
```

需要拼音库（如 `pinyin` crate）把汉字转拼音再取韵母。**注意**：这会引入新依赖，按铁律 Cargo.toml 改动也要走 A→B + Reviewer 审查。

### 5.4 事件推前端

网关 WS 已透传 `extension_event`。LyricExtension 在检测后 `emit_extension_event(ExtensionEvent { custom_type: "LyricIssues", ... })`，前端订阅该 customType 即可实时渲染高亮（不再等 agent_end）。

### 5.5 A→B 执行命令

```bash
ion --host --agent coordinator "按 docs/design/LYRIC_SYSTEM.md §5 实现 Phase 2 内置 lyric_extension：
1. 新增 src/lyric_extension.rs（十三辙押韵 + 音节检测 + on_context 注入 + on_gate_check 拦截）
2. 新增 LyricCheckTool（LLM 语义审查）
3. 接入 build_tools 和 extension 注册
4. 加拼音依赖到 Cargo.toml
5. 补单元测试（十三辙分组 + 音节统计 + XML 注入）
6. cargo build + cargo test --lib 全过"
```

## 6. 验证

### 6.1 自动化 CI（`tests/lyric_webui_ci.sh`）

两层覆盖，共 19 项断言：

**默认模式（不调真 LLM，验证网关协议层）：**
```bash
bash tests/lyric_webui_ci.sh   # 12 项
```
| 组 | 覆盖 |
|----|------|
| Group A | build + host + 网关 + healthz + 静态首页 + 路径逃逸防护 |
| Group B | RPC 转发（create_session / list_sessions / id 透传） |
| Group C | 错误处理（非法 JSON → 400 / 未知路由 → 404） |

**E2E 模式（调真实 glm-5.2，验证完整链路）：**
```bash
ION_E2E=1 bash tests/lyric_webui_ci.sh   # +7 项，共 19 项
```
| 组 | 覆盖 |
|----|------|
| Group D | WebSocket 流式透传 — 断言收到 text_delta（实测 563 个）或 tool_call |
| Group E | lyricist 真实改编 — 断言产出 `<lyric_result>` + `<adapted>` 逐句对照 + rhyme_check 块 |
| Group F | critic 真实审查 — 断言产出 `VERDICT: APPROVE/REQUEST_CHANGES` |

E2E 通过 `get_messages` RPC 提取 assistant 文本断言（不依赖 session jsonl 文件路径）。

### 6.2 手动验证（网页）
```bash
bash webui/start.sh
# → 浏览器打开 http://localhost:8787
# → 输入原歌词 + 主题 → 点「开始改编」
# → 看到：实时流 → 工具 chip → 对照表 → 押韵/音节检查 → critic VERDICT
```

### 6.3 已知的环境坑（排查用）

- **测试前必须清旧 session**：本机长期累积的脏 session（数千个）+ `last_session` 复用会让 `create_session` 接到旧上下文，导致 agent 行为跑偏。清理：`rm -rf ~/.ion/agent/sessions/ ~/.ion/agent/last_session`（AGENTS.md 明确授权）。
- **watchdog 占用 socket**：`scripts/watchdog.sh --monitor` 会自动拉起 host，抢占 `~/.ion/host.sock`，导致新 host 起不来（报 `Host already running`）。调试前先 `pkill -f "scripts/watchdog"`。
- CI 脚本已内置 `cleanup_stale_host`（按 pid 文件 + socket 精确清理，不用宽泛 pkill，符合 AGENTS.md 规范）。
