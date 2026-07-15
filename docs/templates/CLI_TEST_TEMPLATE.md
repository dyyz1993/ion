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
4. **Group 分组按用户场景，不按 RPC/技术维度**（详见下方方法论）
5. **预期 JSON 必须是完整的**（包含 `type` / `id` / `command` / `success` / `data`）

---

## 测试组设计方法论（必读）

> 写测试组之前**必须读这一节**。不是想到什么测什么，而是按以下步骤系统性设计。

### 第一步：列出核心链路（最重要）

核心链路 = **用户使用这个功能时，从头到尾走过的关键路径**。不是技术维度（精度/延迟/token），是用户真实操作序列。

**怎么找核心链路**：问自己"用户拿这个功能干什么？"每个答案就是一条链路。

**示例**（以 Memory 为例）：
- "存了一条，之后能找到" → 链路：save → search → 注入
- "换了项目还能找到" → 链路：项目A save → 项目B search → 跨项目召回
- "存太多不会撑爆" → 链路：存100条 → 注入时只取5条 → token可控
- "重复的自动清理" → 链路：存重复 → consolidate → 去重

**反例**（不要这样分）：
- ❌ Group A: 精度 / Group B: 延迟 / Group C: token —— 这是技术维度，不是用户场景
- ❌ Group A: save RPC / Group B: search RPC —— 这是按 RPC 分，不是按场景分

### 第二步：每条链路设计 3 层 case

对每条核心链路，从三个层次设计 case：

| 层次 | 问什么 | 举例 |
|------|--------|------|
| **能不能用**（Happy path） | 正常操作，功能通不通？ | 存了"认证用 DeepSeek"→搜"认证"→找到了 |
| **好不好用**（质量） | 结果准不准/快不快/贵不贵？ | 搜"认证"不会误搜出"天气"；延迟 < 50ms |
| **会不会出问题**（边界） | 极端情况崩不崩？ | 空库搜索不 panic；特殊字符不破坏 XML |

### 第三步：准备真实测试数据

**不要用 "test memory 1" / "hello world" 这种无意义数据**。用模拟真实场景的数据：

```
✅ 好："认证用 DeepSeek API，key 在 auth.json"（category=设计决策, project=ion）
❌ 坏："test data 1"（没有任何语义，测不出精度问题）
```

测试数据应该覆盖：
- 多个项目（测跨项目）
- 多个类别（测分类/过滤）
- 不同 importance（测排序）
- 包含容易混淆的条目（测精度——"认证 API" vs "认证 token"）

### 第四步：Group 命名用用户场景，不用技术术语

```
✅ Group A：存了能找到
✅ Group B：跨项目回忆
✅ Group C：不卡用户
❌ Group A：检索精度（Recall + Precision）
❌ Group B：注入延迟（Latency）
```

### 第五步：每个 case 的输入用用户自然语言

```
✅ INPUT='认证 API key 在哪'       （用户真会这么说）
❌ query='认证' precision_threshold=0.9  （技术参数，用户不会说）
```

### 完整示例：Memory Active 的 Group 结构

```
核心链路分析：
  1. 存了能找到（最核心）
  2. 跨项目回忆（V0.2 核心价值）
  3. 不卡用户（性能）
  4. 不撑爆上下文（成本）
  5. 自动整理（长期可用性）
  6. 边界安全（健壮性）

Group 设计（每条链路一层 Group）：
  Group A：存了能找到（5 case = 能用3 + 好用1 + 边界1）
  Group B：跨项目回忆（3 case = 能用2 + 好用1）
  Group C：不卡用户（4 case = 好用4，不同数据量延迟）
  Group D：不撑爆上下文（4 case = 好用4，token 上限）
  Group E：自动整理（5 case = 能用2 + 好用2 + 边界1）
  Group F：边界安全（5 case = 边界5）
```

### 检查清单（写完测试组后自查）

- [ ] 每条核心链路都有对应的 Group？
- [ ] 每个 Group 至少有一个"能不能用"的 Happy path case？
- [ ] 每个 Group 至少有一个"好不好用"的质量 case？
- [ ] 有专门的边界/安全 Group？
- [ ] 测试数据是模拟真实场景的（不是 test1/test2）？
- [ ] case 的输入是用户自然语言（不是技术参数）？
- [ ] Group 命名是用户场景（不是技术维度）？
- [ ] 有性能/成本相关的可测量指标？（延迟 ms / token 数 / 结果条数）
