# LSP Extension CLI 测试指南

> **状态：🔧 设计稿** — case 已设计完成，待代码实现后跑通。
>
> 本文档是**纯 CLI 验证用例**（给 QA/写 CI 脚本的人看），含完整命令 + 请求/响应 JSON + 验证点。

---

## RPC 接口规格

### `lsp_check` 工具

**请求（via call_tool）：**

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"lsp_check","args":{}}'
```

**请求参数：** 无（自动检测 Cargo.toml + 跑 cargo check）

**响应（有错误）：**

```
📋 Diagnostics (2 issues):

🔴 src/lib.rs:42:5 [E0308] mismatched types
🟡 src/main.rs:10:1 [unused_variables] unused variable: `x`

Status: 2 issues (1 error, 1 warning)
```

**响应（无问题）：**

```
✅ No diagnostics. Project compiles cleanly.
```

---

### `extension_rpc lsp check`

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**请求参数：** 无

**响应 JSON（成功，有诊断）：**

```json
{
  "type": "response",
  "id": "rpc-client",
  "success": true,
  "data": {
    "count": 1,
    "has_errors": true,
    "diagnostics": [
      {
        "file": "src/lib.rs",
        "line": 42,
        "column": 5,
        "severity": "error",
        "code": "E0308",
        "message": "mismatched types"
      }
    ]
  }
}
```

**响应 JSON（成功，无诊断）：**

```json
{
  "success": true,
  "data": {
    "count": 0,
    "has_errors": false,
    "diagnostics": []
  }
}
```

---

### `extension_rpc lsp status`

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
    "diagnostic_count": 0
  }
}
```

---

### `extension_rpc lsp clear`

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"clear"}'
```

**响应 JSON：**

```json
{"success": true, "data": {"cleared": true}}
```

---

## Group A: 基础功能（cargo check + diagnostics 返回）

### A1: extension_rpc lsp check — 有编译错误

```bash
# 准备：写一个有错误的文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"src/bad.rs","content":"fn main() { let x: String = 42; }"}}'

# 触发诊断
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**验证点：**
- ✅ 返回 `count >= 1`
- ✅ `has_errors == true`
- ✅ diagnostics 包含 `severity: "error"`
- ✅ file 指向 `src/bad.rs`

### A2: extension_rpc lsp check — 编译通过

```bash
# 先修复
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"src/bad.rs","content":"fn main() { let x: String = String::new(); }"}}'

# 再检查
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**验证点：**
- ✅ 返回 `count: 0`
- ✅ `has_errors: false`
- ✅ `diagnostics: []`

### A3: extension_rpc lsp status

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"status"}'
```

**验证点：**
- ✅ `enabled: true`
- ✅ `diagnostic_count` 字段存在

---

## Group B: 自动注入（write → 检测 → context）

### B1: write 后自动触发 cargo check

```bash
# 用 FauxProvider 让 agent 调 write（有错误代码）
# 然后检查 agent 的下一轮 context 是否包含 <diagnostics>

# 在 serve 日志里验证
grep "\[lsp\]" /tmp/serve.log | head -5
```

**验证点：**
- ✅ 日志含 `[lsp] write/edit detected, marking dirty`
- ✅ 日志含 `[lsp] cargo check completed: N diagnostics`

### B2: <diagnostics> XML 注入到 context

```bash
# 用 FauxProvider 驱动 agent loop
# 检查 agent 的 messages 里是否有 <diagnostics> XML

ion rpc --session <sid> --method get_messages 2>/dev/null | grep "<diagnostics>"
```

**验证点：**
- ✅ messages 包含 `<diagnostics>` XML block
- ✅ XML 里有 `file/line/code/message` 属性

### B3: edit 后自动触发

```bash
# 用 edit 工具（不是 write）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"edit","args":{"file_path":"src/lib.rs","old_string":"fn main()","new_string":"fn not_main()"}}'

# 验证 dirty 标记
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"status"}'
```

**验证点：**
- ✅ `dirty: true`（edit 也触发）

---

## Group C: LLM 主动查询（lsp_check 工具）

### C1: call_tool lsp_check

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"lsp_check","args":{}}'
```

**验证点：**
- ✅ 返回 diagnostics 格式化文本
- ✅ 有 `🔴` (error) 或 `🟡` (warning) 前缀
- ✅ 无问题时返回 `✅ No diagnostics`

### C2: lsp_check 在 tool_defs 里可见

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"lsp_check","args":{}}'
```

**验证点：**
- ✅ 工具存在（不返回 "tool not found"）

---

## Group D: 边界场景

### D1: 无 Cargo.toml（非 Rust 项目）

```bash
# 在没有 Cargo.toml 的目录启动 serve
cd /tmp/no-rust-project
ion serve &
ion rpc --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**验证点：**
- ✅ 返回 `count: 0`（跳过，不是报错）
- ✅ 日志含 `[lsp] no Cargo.toml found, skipping`

### D2: cargo check 超时

```bash
# 模拟超时（大项目或 IO 慢）
# 设置 ION_LSP_TIMEOUT=1（1 秒超时）
ION_LSP_TIMEOUT=1 ion serve &
ion rpc --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**验证点：**
- ✅ 不崩溃
- ✅ 返回 `timeout: true`

### D3: extension_rpc lsp clear

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"clear"}'
```

**验证点：**
- ✅ 返回 `cleared: true`
- ✅ 之后 status 显示 `diagnostic_count: 0`

---

## Group E: extension_rpc 全方法

### E1: check 方法

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"check"}'
```

**验证点：**
- ✅ success: true
- ✅ data.count 字段存在

### E2: status 方法

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"status"}'
```

**验证点：**
- ✅ success: true
- ✅ data.enabled / data.dirty 字段存在

### E3: clear 方法

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"lsp","method":"clear"}'
```

**验证点：**
- ✅ success: true
- ✅ data.cleared: true

---

## CI 脚本结构建议

`tests/lsp_ci.sh` 按 Group 组织：

```bash
# Group A: 基础功能（3 case）
# Group B: 自动注入（3 case，需要 FauxProvider）
# Group C: LLM 工具（2 case）
# Group D: 边界（3 case）
# Group E: extension_rpc（3 case）
# 合计 14 case
```
