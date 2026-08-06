# Bug: tool_loop_detector UTF-8 panic

**发现时间**: 2026-07-31
**触发场景**: ion agent 处理含中文的 prompt 时，调 `write` 工具（或其它落到 `_` 分支的工具）
**严重度**: 高（agent 直接 crash，exit 101）

## 错误

```
thread 'main' panicked at src/tool_loop_detector.rs:120:30:
end byte index 100 is not a char boundary; it is inside '名' (bytes 98..101 of string)
```

## 根因

`src/tool_loop_detector.rs:120`:

```rust
let truncated = if args_str.len() > 100 {
    &args_str[..100]   // ❌ byte slice，中文 UTF-8 多字节会切到字符中间
} else {
    &args_str
};
```

`&str[..100]` 是按字节切的，args 含中文时（如 prompt 里写"蒙娜丽莎"），第 100 字节可能落在某个汉字 UTF-8 序列中间（每个汉字 3 字节），导致 panic。

## 修复方案

用字符边界安全的截断。任选其一：

```rust
// 方案 A：用 chars().take()
let truncated: String = args_str.chars().take(100).collect();
format!("{tool_name}:{truncated}")

// 方案 B：用 floor_char_boundary（nightly）
// 方案 C：手动找最近的 char boundary
let mut end = 100.min(args_str.len());
while end > 0 && !args_str.is_char_boundary(end) {
    end -= 1;
}
&args_str[..end]
```

推荐方案 A（最简单，stable Rust 可用）。

## 影响范围

只在 `_` 分支（非 bash/grep/find/spawn_worker 等已特殊处理的工具）触发。`write` / `edit` / MCP 工具等都走这个分支。

## 临时 workaround

prompt 用纯英文（每字符 1 字节，不会跨边界）。

## 修复任务规格（给 A→B 流程）

### 改动点
文件：`src/tool_loop_detector.rs`，`compute_signature` 方法的 `_` 分支（约 116-125 行）。

把：
```rust
let truncated = if args_str.len() > 100 {
    &args_str[..100]
} else {
    &args_str
};
format!("{tool_name}:{truncated}")
```

改成 UTF-8 字符边界安全的截断：
```rust
let truncated: String = args_str.chars().take(100).collect();
format!("{tool_name}:{truncated}")
```

### 测试要求（必须加）
在 `src/tool_loop_detector.rs` 的 `#[cfg(test)] mod tests` 里加 3 个单元测试：

1. `test_signature_truncates_long_ascii` — 100+ 字符的纯 ASCII args，签名被截断到合理长度，不 panic
2. `test_signature_handles_multibyte_utf8` — args 含中文（如 `{"text":"蒙娜丽莎的微笑蒙娜丽莎..."}` 重复到 >100 字节），签名计算不 panic，返回有效 String
3. `test_signature_handles_emoji_and_mixed` — args 含 emoji + 中英混合（如 `{"x":"😀😊猫猫cat"}`），不 panic

### 验证清单（developer 自跑）
- [ ] `cargo test --lib tool_loop_detector` 全过
- [ ] `cargo build` 成功
- [ ] `cargo clippy` 无新增 warning
- [ ] 手动验证：用中文 prompt 跑 ion 不再 panic（可选，需 LLM）

## 待办

按 A→B 铁律，走 ion --host --agent coordinator 让 B 修。
