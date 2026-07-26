# LSP Extension 设计文档 — cargo check 诊断集成

> **状态：🔧 设计稿** — 对齐 pi 的 LSP 扩展（精简版：cargo check 替代完整 LSP server）。

---

## 何时使用这个文档

- 给 LLM 提供编译/类型错误反馈时
- 让 developer agent 写代码后自动看到 diagnostics 时
- 对齐 pi 的 `extensions/lsp/`（精简版）时

**触发时机**：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

**参考样本**：
- [docs/design/BASH_EXTENSION.md](./BASH_EXTENSION.md) — Bash 扩展（Tool + Extension + RPC + CLI 测试）
- [docs/design/MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md) — Memory 扩展（on_context 注入模式）

---

## 概览

LLM 写代码后不再盲改。LSP Extension 在每次 `write`/`edit` 工具调用后自动跑 `cargo check --message-format=json`，解析编译器消息（errors + warnings），在下一轮 LLM 调用前通过 `on_context` 钩子注入 `<diagnostics>` XML block。LLM 看到编译错误后自己修。

对齐 pi 的 `extensions/lsp/`（1657 行 client + 402 行 tool），但用 `cargo check` 替代完整 rust-analyzer JSON-RPC server——覆盖 80% 场景（编译错误 + 警告），代码量 ~300 行 vs pi ~2060 行。

| 能力 | 入口 | 状态 |
|------|------|------|
| cargo check 自动触发 | `on_tool_execution_end`（write/edit 后） | 🔧 |
| diagnostics 解析 | `cargo check --message-format=json` → Diagnostic | 🔧 |
| context 注入 | `on_context` → `<diagnostics>` XML | 🔧 |
| LLM 主动查询 | `lsp_check` tool | 🔧 |
| CLI 直调 | `extension_rpc lsp check/clear/status` | 🔧 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | Diagnostic struct（file/line/col/severity/message/code） | 🔧 | `cargo test --lib lsp` |
| 1.2 | cargo check JSON 解析 | 🔧 | `cargo test --lib lsp::test_parse` |
| 1.3 | XML 格式化注入 | 🔧 | `cargo test --lib lsp::test_format` |
| 2.1 | LspExtension on_tool_execution_end | 🔧 | `lsp_ci.sh Group B` |
| 2.2 | LspExtension on_context 注入 | 🔧 | `lsp_ci.sh Group B` |
| 2.3 | LspExtension on_extension_rpc | 🔧 | `lsp_ci.sh Group E` |
| 3.1 | LspCheckTool impl Tool | 🔧 | `lsp_ci.sh Group C` |
| 3.2 | LspCheckTool 参数 schema | 🔧 | tool_defs JSON |
| 4.1 | config.json extensions.lsp.enabled 开关 | 🔧 | `lsp_ci.sh Group D` |
| 4.2 | 无 Cargo.toml 时不触发 | 🔧 | `lsp_ci.sh Group D` |
| 5.1 | 编译通过时注入 diagnostics count=0 | 🔧 | `lsp_ci.sh Group D` |
| 5.2 | cargo check 超时（120s）处理 | 🔧 | `lsp_ci.sh Group D` |
| 6.1 | 注册到 worker_rpc.rs | 🔧 | serve 启动日志 `[lsp] initialized` |
| 6.2 | 注册到 ion.rs standalone | 🔧 | `ion --extension lsp` |

---

## 1. 配置

**文件**：`src/lsp_extension.rs`

```rust
/// A single compiler diagnostic from cargo check.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub severity: String,    // "error" | "warning"
    pub message: String,
    pub code: String,        // "E0308" | "unused_variables" etc
}

/// LSP Extension — cargo check diagnostics integration.
pub struct LspExtension {
    /// Current diagnostics (shared with LspCheckTool).
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    /// True = files changed, need re-check before next context injection.
    dirty: Arc<AtomicBool>,
    /// True = last cargo check had errors.
    has_errors: Arc<AtomicBool>,
    name: String,
}
```

默认值：无持久化（每次实时 cargo check）。

**config.json 开关**：

```json
{
  "extensions": {
    "lsp": { "enabled": true }
  }
}
```

默认 **开启**。只在有 `Cargo.toml` 的项目里生效（自动检测）。

---

## 2. 主流程 / 数据结构

**文件**：`src/lsp_extension.rs`

```
┌─────────────────────────────────────────────────────────────┐
│ developer agent                                             │
│   ├─ 调 write/edit 工具改代码                                │
│   ├─ on_tool_execution_end 钩子触发                          │
│   │   └─ tool_name=="write"||"edit" → dirty=true            │
│   │                                                          │
│   ├─ 下一轮 LLM 调用前，on_context 钩子触发                  │
│   │   └─ dirty=true? → 跑 cargo check --message-format=json │
│   │       ├─ 解析 JSON → Vec<Diagnostic>                    │
│   │       ├─ 存入 self.diagnostics                          │
│   │       └─ 注入 <diagnostics> XML 到 messages             │
│   │                                                          │
│   └─ LLM 看到编译错误 → 修代码 → 再次 write/edit → 循环     │
└─────────────────────────────────────────────────────────────┘
```

### 关键决策点

| 场景 | 处理 |
|------|------|
| write/edit 后 | 设 dirty=true（不立即跑 cargo check，等下一轮 on_context） |
| on_context + dirty=true | 跑 cargo check（120s timeout），解析，注入 |
| on_context + dirty=false | 不注入（上次的 diagnostics 已消化） |
| 无 Cargo.toml | 跳过（不跑 cargo check） |
| cargo check 超时 | 注入 `<diagnostics timeout="true">` |
| cargo check 成功（0 errors） | 注入 `<diagnostics count="0" status="clean">` |
| LLM 调 lsp_check 工具 | 实时跑 cargo check，返回格式化结果 |

### cargo check JSON 解析

```bash
cargo check --message-format=json 2>/dev/null
```

每行一个 JSON，只关心 `reason == "compiler-message"` 的行：

```json
{
  "reason": "compiler-message",
  "message": {
    "level": "error",
    "code": { "code": "E0308" },
    "message": "mismatched types: expected `String`, found `&str`",
    "spans": [{
      "file_name": "src/lib.rs",
      "line_start": 42,
      "column_start": 5
    }]
  }
}
```

解析逻辑：
1. 逐行 `serde_json::from_str`
2. 过滤 `reason == "compiler-message"`
3. 提取 level / code.code / message / spans[0].file_name / line_start / column_start

### XML 注入格式

```xml
<diagnostics count="2" has_errors="true">
<error file="src/lib.rs" line="42" col="5" code="E0308">
mismatched types: expected `String`, found `&str`
</error>
<warning file="src/main.rs" line="10" col="1" code="unused_variables">
unused variable: `x`
</warning>
</diagnostics>
```

---

## 3. 接口规格

### 3.1 `lsp_check` LLM 工具

**请求（LLM 调用）：**

```json
{
  "name": "lsp_check",
  "arguments": {}
}
```

**响应（成功）：**

```
📋 Diagnostics (2 issues):

🔴 src/lib.rs:42:5 [E0308] mismatched types: expected `String`, found `&str`
🟡 src/main.rs:10:1 [unused_variables] unused variable: `x`

Run: cargo check
Status: 2 issues (1 error, 1 warning)
```

**响应（无问题）：**

```
✅ No diagnostics. Project compiles cleanly.
```

### 3.2 `extension_rpc lsp check` — CLI 直调

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "rpc-client",
  "success": true,
  "data": {
    "count": 2,
    "has_errors": true,
    "diagnostics": [
      {
        "file": "src/lib.rs",
        "line": 42,
        "column": 5,
        "severity": "error",
        "code": "E0308",
        "message": "mismatched types: expected `String`, found `&str`"
      }
    ]
  }
}
```

### 3.3 `extension_rpc lsp clear` — 清除缓存

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"clear"}'
```

**响应 JSON：**

```json
{"success": true, "data": {"cleared": true}}
```

### 3.4 `extension_rpc lsp status` — 查询状态

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"status"}'
```

**响应 JSON：**

```json
{
  "success": true,
  "data": {
    "enabled": true,
    "dirty": false,
    "has_errors": false,
    "last_check": "2026-07-26T10:00:00Z",
    "diagnostic_count": 0
  }
}
```

---

## 4. CLI 测试指南

> 详细测试 case 见独立的 [LSP_CLI_TEST.md](../testing/LSP_CLI_TEST.md)。

### 测试组概览

| Group | 场景 | case 数 |
|-------|------|--------|
| **A** | 基础功能（cargo check + diagnostics） | 3 |
| **B** | 自动注入（write → 检测 → context） | 3 |
| **C** | LLM 主动查询（lsp_check 工具） | 2 |
| **D** | 边界（无 Cargo.toml / 通过 / 超时） | 3 |
| **E** | extension_rpc CLI 直调 | 3 |
| **合计** | | **14** |

---

## 5. 对标 pi

| 维度 | pi LSP | ION LSP（本设计） |
|------|--------|-------------------|
| 引擎 | rust-analyzer JSON-RPC（完整 LSP server） | cargo check --message-format=json |
| 诊断 | diagnostics + definition + hover + references + rename | 仅 diagnostics（P0） |
| 触发 | 文件保存 + 钩子 | write/edit 后 on_tool_execution_end |
| 注入 | agent_end 自动注入 + lsp 工具 | on_context 注入 + lsp_check 工具 |
| 代码量 | ~2060 行（client 1657 + tool 402） | ~300 行 |
| 多语言 | 通过 LSP server 支持任意语言 | 先只 Rust |
| 精度 | 精确（实时 AST 分析） | 粗（编译后才知道错误） |
| 速度 | 快（增量分析） | 慢（全量 cargo check，3-30s） |

**为什么选 cargo check 而不是 rust-analyzer**：
1. cargo check 不需要长连接管理（rust-analyzer 要 spawn + JSON-RPC）
2. cargo check 输出标准化（JSON 格式稳定）
3. 覆盖 80% 场景（编译错误 + 警告 = LLM 最需要的反馈）
4. 300 行 vs 2060 行（投入产出比高）
5. 后续可升级到 rust-analyzer（on_tool_execution_end 换成 LSP notify）

---

## 6. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | rust-analyzer 集成（增量分析，更快更精确） | P1 |
| 2 | go-to-definition / hover / references | P1 |
| 3 | 多语言支持（TypeScript / Python / Go） | P2 |
| 4 | diagnostics 持久化（跨 session 复用上次结果） | P2 |
| 5 | GateDecision（编译不过时不让 agent 停止） | P2 |
| 6 | clippy 集成（`cargo clippy --message-format=json`） | P1 |
