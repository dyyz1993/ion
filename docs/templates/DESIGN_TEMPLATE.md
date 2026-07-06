# {功能名} 设计文档

> **状态：{已完成 / 已验证 / 开发中 / 暂不开发 / 待定}** — {一句话说明当前进度}。

---

## 何时使用这个模板

启动新功能开发、或对某个子系统做完整设计时使用。一份设计文档应覆盖：能力清单 → 配置 → 主流程 → 接口规格 → CLI 测试 → bug fix 记录。

**触发时机**：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

**参考样本**（已使用此模板的文档）：
- [docs/design/COMPACTION.md](../design/COMPACTION.md) — Compaction 会话压缩系统
- [docs/design/PROVIDER_PROTOCOL.md](../design/PROVIDER_PROTOCOL.md) — 多 Provider 协议实现
- [docs/design/BASH_EXTENSION.md](../design/BASH_EXTENSION.md) — Bash 进程管理扩展

---

## 概览

{一两段话说明这个功能做什么、解决什么问题、对齐 pi 的哪个模块。}

| 能力 | 入口 | 状态 |
|------|------|------|
| {能力 1} | `{入口 RPC/工具/钩子}` | ✅ |
| {能力 2} | `{入口}` | ✅ |
| {能力 3} | `{入口}` | 🔧 设计稿 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | {子功能} | ✅ | `{cargo test / ion rpc 命令}` |
| 1.2 | {子功能} | ✅ | `{验证命令}` |
| 2.1 | {子功能} | 🔧 | 待实现 |

---

## 1. 配置

**文件**：[src/xxx.rs#L1-L10](file:///Users/xuyingzhou/Project/study-rust/ion/src/xxx.rs#L1-L10)

```rust
pub struct XxxConfig {
    pub field1: usize,
    pub field2: String,
}
```

默认值：`field1=100` / `field2="default"`

---

## 2. 主流程 / 数据结构

**文件**：[src/xxx.rs#L20-L50](file:///Users/xuyingzhou/Project/study-rust/ion/src/xxx.rs#L20-L50)

```rust
pub async fn xxx(...) -> Result<XxxResult> {
    // Step 1: ...
    // Step 2: ...
    // Step 3: ...
}
```

### 关键决策点

| 场景 | 处理 |
|------|------|
| {场景 1} | {处理方式} |
| {场景 2} | {处理方式} |

---

## 3. 关键 bug fix 记录

> 历史上踩过的坑，写清楚"修复前 vs 修复后"，避免回退。

### Bug 1：{标题}

**文件**：[src/xxx.rs#L100-L110](file:///Users/xuyingzhou/Project/study-rust/ion/src/xxx.rs#L100-L110)

**修复前**：{描述 panic / 错误行为}

**修复后**：

```rust
// 关键修复代码
if xxx.is_empty() {
    return Ok(());  // 避免越界
}
```

---

## 4. 接口规格

### 4.1 {RPC 名} 接口

**请求：**

```bash
ion rpc --session <sid> --method {method} \
  --params '{"field1":"value1","field2":123}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `field1` | string | 是 | {说明} |
| `field2` | number | 否 | {说明，默认值} |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "{method}",
  "success": true,
  "data": {"ok": true, "result": "..."}
}
```

**响应 JSON（失败）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "{method}",
  "success": false,
  "error": "..."
}
```

---

## 5. CLI 测试指南

> 详细测试 case 见独立的 CLI 测试指南章节（参考 [CLI_TEST_TEMPLATE.md](./CLI_TEST_TEMPLATE.md)）。

### Group A：{测试主题 1}

```bash
# A1 {用例名}
ion rpc --session <sid> --method {method} --params '{...}'
```

**预期：** {期望结果}

### Group B：{测试主题 2}

...

---

## 6. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | {待办 1} | P1 |
| 2 | {待办 2} | P2 |
