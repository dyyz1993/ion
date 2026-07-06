# Memory 扩展设计

> **状态：设计稿** — 尚未实现，等待确认后开发。

## 概述

项目级别的记忆扩展。自动维护大纲索引，根据用户输入异步检索相关记忆，通过 `on_context` 钩子在每次发 LLM 前直接注入 custom entry，**不经过 follow_up**。

## 评审重点（待拍板）

| # | 问题 | 建议方案 |
|---|------|---------|
| 1 | **注入时机**：走 `on_context` 还是 `follow_up`？ | ✅ **on_context** — 发 LLM 前直接塞 messages |
| 2 | **异步语义**：检索是否阻塞主 LLM？ | ✅ **不阻塞**，本轮来不及则下一轮注入 |
| 3 | **去重策略**： `file_hash + injected_at_turn`？ | ✅ hash 变 **或** 超过 20 轮 → 重注入 |
| 4 | **注入粒度**：默认完整 outline 注入？ | ✅ 超 8000 字符降级为命中条目 top-20 |
| 5 | **整理策略**：consolidation 软删除？ | ✅ 只 `archived: true`，不硬删 |
| 6 | **并发写入**：project memory 目录级锁？ | ✅ outline + injected.json 都锁 |
| 7 | **工具暴露**：LLM 看到什么？ | ✅ `memory_save` / `memory_search`  |
| 8 | **消息优先级**：`<memory_context>` 是上下文还是指令？ | ✅ 是上下文，不是新指令 |

自动维护大纲索引，根据用户输入异步检索相关记忆，通过 `on_context` 钩子在每次发 LLM 前直接注入 custom entry，**不经过 follow_up**。

```
用户输入
  │
  ├──→ 同步：agent.run() 正常处理 LLM 调用
  │
  └──→ 异步：Memory 扩展 on_input 触发
         ├── 读取大纲索引 → 匹配 → 找到相关大纲
         ├── 加载匹配大纲的条目文件
         ├── 计算文件 hash → 对比已注入记录
         ├── hash 没变 → 跳过
         └── hash 变了 → 标记"待注入"

下一次 LLM 调用前：
  on_context 钩子 → 检查"待注入"队列
  → 有 → 直接 push 一条 user message 到 messages
  → 消息体：<memory_context>XML</memory_context>
  → LLM 无缝看到，不经过 follow_up
```

## 关键区别（相对于第一版）

| 特性 | 旧设计（错误） | 新设计（正确） |
|------|-------------|--------------|
| 注入时机 | `agent.follow_up()` | `on_context` 钩子，发 LLM 前直接塞 |
| 消息路径 | follow_up 队列 → 下一轮 run | 直接追加到 messages，下一个 LLM 调用即可见 |
| 去重依据 | 条目 ID | **文件 hash**（文件内容变才重注入） |
| 持久化 | 记录已注条目 ID | 记录 `{文件路径, 文件 hash, 最后注入时间}` |

## 实现方式

**Rust 扩展（AgentExtension trait）**，非 WASM 扩展。原因：
- 需要 async 文件 I/O
- 需要 `on_context` 和 `on_input` 两个生命周期钩子
- 需要在内存维护"待注入"队列

## 存储维度

使用 project / session 两个维度：

| 维度 | 路径 | 用途 |
|------|------|------|
| **project** | `~/.ion/agent/project-data/{hash}--{name}/memory/` | 大纲索引 + 条目（主存储） |
| **session** | `~/.ion/agent/sessions/{hash}/data/{sid}/memory/` | 已注入记录（文件 hash 历史） |

## 文件结构

```
~/.ion/agent/project-data/{hash}--{name}/memory/
├── index.json                    ← 所有大纲的概要列表
└── outlines/
    ├── preferences.json          ← 偏好类条目
    ├── project.json              ← 项目类条目
    ├── technical.json            ← 技术决策类
    └── <custom>.json             ← 用户自定义

~/.ion/agent/sessions/{hash}/data/{sid}/memory/
└── injected.json                 ← 去重记录 {path, hash, injected_at}
```

## 数据格式

### 条目格式（入仓格式）

每条记忆有固定的结构化格式，不是自由文本：

```json
{
  "id": "mem_1",
  "content": "偏好使用 Rust 和 TypeScript，避免使用 JavaScript",
  "description": "用户的语言偏好说明",
  "category": "编程语言偏好",
  "tags": ["rust", "typescript", "语言"],
  "outline": "preferences"
}
```

| 字段 | 说明 | 用途 |
|------|------|------|
| `content` | 实际记住的内容 | 最终注入到 LLM 的正文 |
| `description` | 概要描述 | 检索匹配时快速过滤 |
| `category` | **分类** | 记忆的类别名称（如"编程语言偏好""API规范""架构决策"） |
| `tags` | 标签词列表 | 用户输入匹配 tag → 命中该记忆 |
| `outline` | 所属大纲文件 | 组织层级，决定存到哪个文件 |

**检索逻辑**：用户输入 → 分词 → 匹配 `tags` + `category` + `description` → 命中 → 注入

### 条目文件 (`outlines/preferences.json`)

```json
[
  {
    "id": "mem_1",
    "content": "偏好使用 Rust 和 TypeScript",
    "description": "用户的语言偏好",
    "tags": ["rust", "typescript"]
  },
  {
    "id": "mem_2",
    "content": "使用 clash 作为 API 规范",
    "description": "API 设计规范",
    "tags": ["api", "clash", "规范"]
  }
]
```

### 大纲分类（N 种，固定但可扩展）

| 大纲 ID | 说明 |
|---------|------|
| `preferences` | 用户偏好（语言、工具、习惯） |
| `project` | 项目信息（目标、范围、约束） |
| `technical` | 技术架构（选型、依赖、方案） |
| ... | 按需扩展，但不宜过多 |

### 已注入记录 (session 维度，`injected.json`)

```json
[
  {
    "path": "preferences",
    "file_hash": "abc123...",
    "injected_at_turn": 12,
    "last_injected_at": 1700000
  }
]
```

**去重逻辑**（三维判断）：

| 条件 | 行为 |
|------|------|
| 文件 sha256 变了 | ✅ 重新注入（内容更新） |
| sha256 未变，但距上次注入 > 20 轮 | ✅ 重新注入（窗口已滚动） |
| sha256 未变，且距上次注入 ≤ 20 轮 | ❌ 跳过（仍在有效窗口内） |

`injected.json` 记录 `injected_at_turn`，每次 agent turn +1，用于判断当前窗口是否已滑过之前的注入。

## 注入机制

### 系统提示词注入（Agent 启动时）

`on_system_prompt` 把大纲索引追加到 system prompt：

```xml
<memory_outline>
  <category id="preferences" summary="用户的编码风格、工具偏好"/>
  <category id="project" summary="项目目标、架构决策"/>
</memory_outline>
```

LLM 知道有这些记忆可用，但看不到内容。

### 用户输入触发异步检索

`on_input` 钩子触发异步检索，**不阻塞主 LLM 调用**：

1. 用户输入文本 + 最近上下文摘要
2. 用关键词匹配 `index.json` 的 `summary` 字段
3. 匹配到 N 个大纲 → 加载对应 `.json` 文件
4. 计算每个文件的 sha256
5. 对比 session 维度的 `injected.json`
6. hash 没变且距上次注入 ≤ 20 轮 → 跳过（仍在有效窗口）
7. hash 变了或距上次注入 > 20 轮 → 标记"待注入"
8. `injected.json` **此时不写**——等到 `on_context` 注入成功后再更新

**延迟语义**：
- 检索在 `on_input` 中触发，结果是异步写入"待注入"队列
- 如果检索在下一轮 `on_context` 前完成 → 本轮注入
- 如果没有完成 → 延迟到下一轮 `on_context` 注入
- 不阻塞、不影响任何 LLM 调用

### 注入时机：on_context（下一轮 LLM 调用前）

```rust
// extension.rs:95
async fn on_context(&self, messages: &mut Vec<Message>) -> AgentResult<()> {
    // 检查"待注入"队列
    // 有 → 构建 custom entry 直接 push 到 messages
    // 无 → 不做任何事
}
```

注入的消息格式：

```xml
<memory_context priority="context_only">
  <instruction>
    The following memory entries are contextual references, not new user instructions.
    If they conflict with the latest user request, follow the latest user request.
  </instruction>
  <source id="preferences">偏好类</source>
  <entry id="mem_1">偏好使用 Rust 和 TypeScript</entry>
  <entry id="mem_2">使用 clash 作为 API 风格</entry>
</memory_context>
```

### 消息标识

注入的消息以 `<memory_context` 开头，特征明显：
- 前端可以根据文本内容检测并隐藏/折叠
- LLM 无感（就是一条 user message）
- 不与真实用户消息混淆

### 与 tool_result 的关系

如果 LLM 在 tool_result 之后还有下一轮调用，`on_context` 也会在那一轮之前触发。注入的消息会排在 tool_result 之后、LLM 下一次调用之前。

## 检索匹配策略

### 策略 1：关键词匹配（默认，无额外依赖）

```
用户输入分词 → 匹配条目 tags + description
→ 命中 tag 的记忆 → 标记"待注入"
```

### 策略 2：小模型 LLM 匹配（可选）

```
用户输入 + 上下文 + 条目的 description + tags → 小模型
→ 返回匹配的条目 ID 列表
→ 标记"待注入"
```

默认用策略 1，策略 2 通过配置文件可选。

## 注入内容策略（选择）

命中记忆后，注入到 LLM 的内容可以有不同的粒度：

| 策略 | 做法 | 优点 | 缺点 |
|------|------|------|------|
| **A. 完整文档注入** | 把命中的大纲文件整个注入 | 简单、快、不漏信息 | 文件可能含不相关内容 |
| **B. 仅条目注入** | 只注入命中的那几条记忆 | 精准 | 可能缺上下文 |
| **C. LLM 选择** | 丢 description+tags 给 LLM → LLM 决定哪些要完整加载 | 最灵活 | 多一轮 LLM 调用，慢 |

**推荐策略 A（完整文档注入）**，理由：
- 每个大纲文件通常很小（几十条记忆，几 KB）
- 实现最简单，不需要额外的 LLM 选择步骤
- LLM 有能力从完整文档中自行找到相关信息
- 即使有不相关的内容，LLM 会忽略

**硬限制**：

| 限制 | 值 | 超限行为 |
|------|-----|---------|
| 单条 content 最大长度 | 2000 字符 | 截断 |
| 单次注入最大条目数 | 20 条 | 按 relevancy 排序后取 top-20 |
| 单次注入最大总字符 | 8000 字符 | 降级为策略 B（仅注入命中条目，不注整个文件） |
| 单文件最大条目数 | 200 条 | 超出时拆分为子文件 |

**降级链**：
```
命中 N 条
  ├── N ≤ 20 且总字符 ≤ 8000 → 策略 A：整文件注入
  ├── N ≤ 20 但总字符 > 8000 → 策略 B：仅注入命中的 N 条
  └── N > 20                → 按 relevancy 取 top-20 注入（仍走策略 B）
```

如果后续发现 8000 字符仍太大，再考虑策略 C（LLM 选择）。

## 工具（LLM 可调用）

LLM 只有两个工具，**没有 list/forget/outline 等管理工具**——记忆是自动管理的：

| 工具 | 参数 | 说明 |
|------|------|------|
| `memory_save(content, description, tags, outline?)` | `content`: 记忆内容, `description`: 概要, `tags`: 标签数组, `outline?`: 所属大纲（默认 auto） | 主动保存一条记忆 |
| `memory_search(query)` | `query`: 搜索关键词 | 主动搜索相关记忆 |

**不提供** `memory_list` / `memory_forget` / `memory_outline_list` 等管理类工具。

`memory_save` 的参数严格 JSON：
```json
{
  "content": "偏好使用 Rust 和 TypeScript",
  "description": "用户的语言偏好",
  "tags": ["rust", "typescript"],
  "outline": "preferences"
}
```



### `memory_save` 保存约束

LLM 不应该随意保存，需要约束：

**可以保存：**
- 用户明确说"记住"/"保存"/"以后都这样"
- 长期稳定的偏好（语言、工具、编码风格）
- 项目级事实（目标、架构、约束）
- 技术决策及理由
- 反复出现的约束

**不要保存：**
- 临时任务状态
- 一次性聊天内容
- 未确认的推测
- 敏感信息（密码、密钥）
- LLM 自己猜测的结论

LLM 在调用 `memory_save` 前应评估内容是否符合上述规则。不符合则拒绝保存。

### 三种触发方式

| 方式 | 触发 | 时机 |
|------|------|------|
| **被动检索** | 用户输入 → `on_input` 钩子 | 每次用户消息后自动 |
| **主动搜索** | LLM 调 `memory_search` 工具 | LLM 按需调用 |
| **主动保存** | LLM 调 `memory_save` 工具 | LLM 按需调用 |
| **整理记忆** | 定时任务（每 N 轮触发） | 扩展后台自动 |

## 扩展事件记录（Custom Entry）

框架只分**两类**，`customType` 由扩展自己定，框架不预定义：


### Custom Entry 通用字段

所有 custom entry 统一包含以下字段（`customType` 之外）：

| 字段 | 说明 | 示例 |
|------|------|------|
| `source` | 来源扩展 | `"memory"` |
| `visibility` | `"llm_and_ui"` 或 `"ui_only"` | 框架据此判断是否注入 LLM |
| `correlation_id` | 关联 ID，方便追踪 | `"mem_inject_001"` |
| `schema_version` | 数据格式版本 | 1 |

### 类型 A：纯展示（不注入 LLM）

写入 session.jsonl 但不发给 LLM。`customType` 随意，UI 据此渲染。

```json
{"type":"custom","id":"xxx","timestamp":"...",
 "customType":"memory_searching",     ← 扩展自定
 "data":{"query":"rust"}}
```

```json
{"type":"custom","id":"xxx","timestamp":"...",
 "customType":"memory_consolidate",   ← 扩展自定
 "data":{"reviewed":12,"removed":2}}
```

### 类型 B：注入 LLM + 记录

同时写入 session 和注入 LLM 上下文。`customType` 扩展自定，UI 据此展示样式。

```json
{"type":"custom","id":"xxx","timestamp":"...",
 "customType":"memory_injected",      ← 扩展自定
 "data":{"source":"preferences","entries":["mem_1"]}}
```

**框架不管 `customType` 的具体值是啥**——扩展自己控制。加新扩展也不需要改框架。

## 记忆整理（ Consolidation ）

扩展每 N 轮（默认 5 轮）自动触发一次整理。

### 流程

```
触发条件：turn_index % 5 == 0
  │
  ├── 读取所有大纲文件的所有条目
  ├── 统计每条条目的访问次数（从 injected.json 读取）
  ├── 读取最近上下文（最后几轮的消息）
  ├── 对每条记忆评分（0-10）：
  │   ├── relevancy：与当前上下文相关度
  │   ├── accuracy：是否仍然准确（有无被后续对话推翻）
  │   └── access_count：被注入次数（热度）
  ├── 评分 < 3 → 标记 `archived: true`（软删除）
  ├── 评分 3-7 → 保留，优化描述
  ├── 评分 > 7 → 保留，提升优先级
  ├── 合并重复/重叠的记忆（保留来源 id 链）
  └── 写入整理结果 + 记录 custom entry（类型 A）

**软删除原则**：不硬删任何记忆。`archived: true` 的记忆不再注入，但在文件中保留。
真正清除需要人工确认或 CLI 管理命令。
```

### 整理结果示例

```json
{"type":"custom","id":"xxx","timestamp":"...",
 "customType":"memory_consolidate",
 "data":{
   "reviewed":15,
   "optimized":3,
   "merged":2,
   "removed":1,
   "score_avg":6.8
 }}
```

整理是**纯扩展级别行为**，与当前对话上下文无直接关系。整理结果 UI 可展示，LLM 不消费。


## 并发安全

多个 session 或 worker 可能同时读写记忆文件，需要保护：

| 场景 | 风险 | 措施 |
|------|------|------|
| 同一 session 的 on_input 和 on_context 并发 | 待注入队列竞争 | 单线程处理 (`&mut self` 保证) |
| 多个 session 写同一个 project outlines | 文件覆盖 | outline 文件写入用 **atomic write**（写 tmp → rename） |
| consolidation 和 memory_save 同时写 | 数据丢失 | **outline + injected.json 写前都加文件锁**（`flock`） |
| 多个 Manager 进程操作同一项目 | 文件冲突 | 第一版不考虑（暂不支持多 Manager） |

**原子写实现**（复用 `plugin.rs` 逻辑）：

```rust
// 写 outline 文件
let tmp = path.join("preferences.json.tmp");
let final_path = path.join("preferences.json");
std::fs::write(&tmp, &data)?;
std::fs::rename(&tmp, &final_path)?;
```
## 完整工作流

```
Agent 启动
  │
  ├── on_system_prompt → 追加 <memory_outline> 到 system prompt
  │
  ├── 用户输入
  │   ├── 同步：LLM 调用（正常流程）
  │   └── 异步：on_input
  │         ├── 读 index.json
  │         ├── 匹配大纲
  │         ├── 加载条目文件
  │         ├── 算 hash → 对比 injected.json
  │         └── hash 变了 → 标记"待注入"
  │
  └── 下一轮 LLM 调用前
      └── on_context
            ├── 有待注入？→ push user message
            └── 无 → 跳过

LLM 看到的消息序列（示例）：
  user:   用户输入"帮我用 Rust 写个模块"
  user:   <memory_context>
            <entry>偏好使用 Rust 和 TypeScript</entry>
          </memory_context>
          ← 无感注入，前端可隐藏
  tool_result: ...（如果有）
  assistant: 好的，我来写这个模块...
```


## 系统管理命令

以下命令供系统管理员/用户使用，**LLM 不可见**。通过 `ion rpc` 直接触发：

```bash
# 列出记忆（按大纲）
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_list","args":{"outline":"preferences"}}'

# 删除单条记忆
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_forget","args":{"id":"mem_1"}}'

# 归档记忆
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_archive","args":{"id":"mem_1"}}'

# 查看单条详情
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_inspect","args":{"id":"mem_1"}}'
```

这些工具注册在 ToolRegistry 但**被 `restrict_tools` 过滤掉**，LLM 看不到。

## CLI 测试流程

```bash
# 1. 启动 Manager
ion manager start

# 2. 添加记忆（RPC 直调，不经过 LLM）
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_save","args":{"outline":"preferences","content":"偏好使用 Rust 和 TS","description":"语言偏好","tags":["rust","ts"]}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_save","args":{"outline":"preferences","content":"使用 clash API 风格","description":"API 规范","tags":["api","clash"]}}'

# 3. LLM 引导创建更多记忆
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"帮我用 memory_save 添加几条项目信息"}}'

# 4. RPC 佐证（memory_search 主动搜索）
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"memory_search","args":{"query":"Rust"}}'

# 5. LLM 使用（prompt 触发检索，自动注入记忆到上下文）
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"写一个 Rust 的 CLI 工具"}}'
# → on_input 匹配到 preferences → on_context 注入记忆
# → LLM 看到"偏好使用 Rust"，回复时自动遵守
```

## 实现计划

| 步骤 | 文件 | 内容 |
|------|------|------|
| 1 | `src/agent/memory.rs` | MemoryExtension: on_system_prompt + on_input + on_context |
| 2 | `src/agent/mod.rs` | pub mod memory; |
| 3 | `src/bin/ion_worker.rs` | 注册 MemoryExtension |
| 4 | 测试 | RPC memory_add → RPC memory_list → LLM prompt → 验证注入 |

