# CLI 测试指南模板

> **用途**：给某个功能写 CLI 级别的验证用例，对齐 [SECURITY_CLI_GUIDE.md](../archive/SECURITY_CLI_GUIDE.md) / [COMPACTION.md §9](../design/COMPACTION.md) / [PROVIDER_PROTOCOL.md §7](../design/PROVIDER_PROTOCOL.md) 的 Group A/B/C/D 格式。

---

## 何时使用这个模板

- 功能完成需要写验证方法时
- DESIGN_TEMPLATE 的"CLI 测试指南"章节展开成独立文档时
- 给 QA / 评审方提供可执行测试清单时

**触发时机**：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

**参考样本**：
- [docs/design/COMPACTION.md §9](../design/COMPACTION.md) — Group A/B/C/D + RPC 接口规格
- [docs/design/PROVIDER_PROTOCOL.md §7](../design/PROVIDER_PROTOCOL.md) — 5 组测试 + 完整请求/响应 JSON
- [docs/design/PERMISSION_SYSTEM.md §十一](../design/PERMISSION_SYSTEM.md) — Group A/B/C/D

---

## 文档结构骨架

### 1. RPC 接口规格（每个 RPC 一节）

**请求：**

```bash
ion rpc --session <sid> --method {method} \
  --params '{"field1":"value1"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `field1` | string | 必填 | {说明} |
| `field2` | number | 30 | {说明} |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "{method}",
  "success": true,
  "data": {"ok": true}
}
```

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `ok` | bool | 是否成功 |
| `result` | string | 结果 |

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

### 2. Group A：{测试主题 1}

> {这个 Group 验证什么}

#### A1 {用例名}

```bash
# 1. {准备步骤}
ion rpc --method create_session --params '{"agent":"developer"}'
# → {"session_id":"sess_xxx", ...}

# 2. {执行}
ion rpc --session sess_xxx --method {method} --params '{...}'
```

**预期：**

```json
{"success": true, "data": {...}}
```

**验证点：**
- ✅ {验证点 1}
- ✅ {验证点 2}

#### A2 {用例名}

...

---

### 3. Group B：{测试主题 2}

#### B1 {用例名}

...

---

### 4. Group C：事件订阅测试

> 通过 `ion subscribe --session <sid>` 观察事件流。

#### C1 {用例名}

```bash
# Terminal 1：订阅
ion subscribe --session sess_xxx

# Terminal 2：触发
ion rpc --session sess_xxx --method prompt --params '{"text":"..."}'
```

**预期事件流（Terminal 1）：**

```
{"type":"event","event":{"type":"agent_start", ...}}
{"type":"event","event":{"type":"text_delta","delta":"..."}}
{"type":"event","event":{"type":"agent_end", ...}}
```

---

### 5. Group D：单元测试 + 集成测试

#### D1 单元测试

```bash
cargo test --lib {module_name}
```

**预期：** `N passed`

#### D2 集成测试

```bash
cargo test --test {test_file}
```

---

### 6. Group E：e2e 真实 API 测试（可选）

> 标记为 `#[ignore]`，需显式启用环境变量。

```bash
ION_E2E_XXX=1 ION_XXX_API_KEY="sk-xxx" \
cargo test -p {crate} --test e2e_real_api -- --ignored --nocapture
```

#### 待补充测试

| # | 测试 | 触发条件 | 验证点 |
|---|------|---------|--------|
| 1 | {测试名} | {需要什么} | {验证什么} |

---

## 写作规范

1. **每个 case 必须给完整的 `ion rpc` 命令**（不能只写"调用 xxx 方法"）
2. **每个 RPC 必须给请求/响应 JSON 规格 + 字段表**（不能只给命令不给 JSON 结构）
3. **每个 case 必须给验证点清单**（✅ 标记）
4. **Group 分组按测试主题**，不按 RPC（一个 Group 可能覆盖多个 RPC 的协作）
5. **预期 JSON 必须是完整的**（包含 `type` / `id` / `command` / `success` / `data`）
