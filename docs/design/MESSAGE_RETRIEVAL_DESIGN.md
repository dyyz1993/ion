# ION 会话消息拉取 UI 设计规格说明书

> 用途：将此文档粘贴到任意大模型/代码编辑器，可自动生成匹配的 HTML 原型。
> 覆盖：12 个核心拉取接口 + 6 种 UI 风格 + 3 层数据架构 + 分页/导航/过滤规则。

---

## 一、数据模型（所有接口共享）

### 1.1 Message（消息/entry，JSONL 最小单位）

```typescript
interface Message {
  entryId: string;        // 如 "msg_012"、"cmp_005"
  type: "message" | "custom" | "custom_message" | "system_event" | "compaction" | "branch_summary" | "leaf_pointer";
  role?: "user" | "assistant" | "toolResult" | "custom";
  content?: string;       // 消息正文（user/assistant/toolResult 都有）
  toolCall?: {             // assistant 调工具时
    tool: "read" | "edit" | "bash" | "grep" | "find" | "ls" | "calculator";
    [key: string]: any;   // 工具参数
  };
  isError?: boolean;      // toolResult 是否失败
  stopReason?: "completed" | "tool_use" | "aborted" | "error" | "max_turns" | "compaction_failed";
  isComplete?: boolean;   // false = 中断（半截消息）
  display?: boolean;      // 旁路数据：是否前端展示
  timestamp?: string;     // ISO 8601
  turnId?: number;        // 所属轮次
}
```

### 1.2 Turn（一轮对话）

```typescript
interface Turn {
  turnId: number;
  userId: string;            // 用户消息 entryId
  assistantId?: string;      // 最后一条 assistant 的 entryId
  userContent: string;       // 用户提问（真实消息正文）
  assistantContent?: string; // 回复正文（真实消息正文）
  keySteps: ToolStep[];      // 工具调用序列，结构化
  toolCallCount: number;
  tokens: { input: number; output: number };
  durationMs: number;
  entryCount: number;        // 本轮的 message entry 数量
  status: "completed" | "aborted" | "error" | "max_turns";
  summary?: string;          // 可选，实时计算，不预存
}

interface ToolStep {
  tool: string;
  [key: string]: any;        // 工具参数
}
```

> **强制约束**：每个 turn（含 abort/error/max_turns）都必须有对应的 turn_summary entry，status 标对应值。abort 时 agent_loop 退出前必须追加 turn_summary，确保 `list_turns` 能看到所有 turn（不会因为中断而漏）。

### 1.3 Tree Node（会话树/分支拓扑）

```typescript
interface TreeNode {
  id: string;                // entryId
  parentId: string | null;
  type: "message" | "compaction" | "leaf_pointer" | "custom";
  turnId?: number;
  label?: string;            // 可读简称
}

interface BranchInfo {
  id: string;                // 该分支末端 entryId
  label: string;
  turnRange: [number, number];
  abandoned?: boolean;       // true = 已被回滚
  active?: boolean;          // true = 当前分支
}

interface TreeResponse {
  nodes: TreeNode[];
  currentLeaf: string | null;
  branches: BranchInfo[];
  compactionPoints: string[];
}
```

### 1.4 Session Meta（会话元信息/统计）

```typescript
interface SessionMeta {
  id: string;
  name?: string;
  firstMessage?: string;
  model: string;
  messageCount: number;
  turnCount: number;
  tokens: { input: number; output: number; cacheRead?: number };
  cost: number;
  durationMs: number;
  errorCount: number;
  compressCount: number;
  createdAt: string;
  updatedAt: string;
  lastEntryId?: string;       // 最后一条 entry 的 ID，前端用于增量拉取判断
  // 血缘
  parentSession?: string | null;  // null = 根会话
  parentType?: "fork" | null;
  parentEntry?: string | null;
  hasChildren?: boolean;
  childCount?: number;
}
```

### 1.5 Live Event（实时事件流）

```typescript
interface LiveEvent {
  type: "agent_start" | "text_delta" | "tool_call" | "tool_result" | "agent_end";
  turnId: number;
  entryId?: string;          // text_delta/tool_call 都带，用于前端按 entryId 聚合
  delta?: string;            // text_delta 的增量文本
  tool?: string;             // tool_call 的工具名
  data?: any;                // tool_result 的内容
}
```

---

## 二、接口定义

### 2.1 list_sessions（会话列表，进入拉取的入口）

**用途**：列出所有会话，带血缘元信息。前端用户挑会话的入口。从 index 读，不扫消息。

**参数**：
| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `after` | string | — | 分页游标(sessionId)，向后翻 |
| `limit` | number | `50` | 每页条数。`0` = 全量 |

**返回**：
```json
{
  "sessions": [SessionMeta],
  "hasMore": true,
  "totalCount": 127,
  "nextCursor": "sess_abc"
}
```

> SessionMeta 里的 parentSession/parentType/parentEntry/hasChildren/childCount 是血缘字段（见 §4 血缘）。根会话 parentSession 为 null。

### 2.2 get_messages（消息流）

**用途**：拉取消息列表（含压缩前）。最核心接口。

**参数**：
| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `view` | string | `"live"` | 视点：live / since_compaction / branch:<leaf_id> / full |
| `after` | string | — | 正向分页游标(entryId)，向后翻页 |
| `before` | string | — | 反向分页游标，向上滚动 |
| `limit` | number | `50` | 每页条数。`0` = 全量 |
| `complete_turn` | bool | `true` | 分页边界自动补齐 turn |
| `include_custom` | string | `"none"` | 旁路数据过滤：none / display_only / all |

**返回**：
```json
{
  "messages": [{ "entryId", "type", "role", "content", "toolCall", "isError", "stopReason", "isComplete" }],
  "hasMore": true,
  "totalCount": 850,
  "nextCursor": "msg_830",
  "view": "live",
  "compactionPoints": [{ "entryId": "cmp_005", "summary": "...", "tokensBefore": 32000 }],
  "pageInfo": { "requestedLimit": 20, "actualCount": 22, "completedTurnBoundary": "msg_022" }
}
```

### 2.3 list_turns（逐轮概览）

**用途**：按 turn 聚合的概览，前端渲染卡片/时间线。**带真实消息正文**（userContent/assistantContent），不是造的摘要。

**参数**：
| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `view` | string | `"live"` | 同 get_messages |
| `after` | string | — | 正向游标（turnId） |
| `before` | string | — | 反向游标（turnId） |
| `limit` | number | `50` | 每页轮数。`0` = 全量 |
| `full_content` | bool | `false` | 默认 content 截断到前 200 字符（超出标 `"..."`）。true = 不截断（注意：用户贴大文件时可能撑大响应） |

**返回**：
```json
{
  "turns": [{
    "turnId": 1, "userId": "msg_001", "userContent": "帮我重构",
    "assistantId": "msg_002", "assistantContent": "好的，我来...",
    "keySteps": [{"tool": "read", "path": "src/session_jsonl.rs"}],
    "toolCallCount": 1, "tokens": {"input": 150, "output": 200},
    "durationMs": 1200, "entryCount": 2, "status": "completed"
  }],
  "hasMore": false, "totalCount": 10, "nextCursor": null
}
```

### 2.4 list_inputs（用户输入列表）

**用途**：极轻量，只返回用户提问。用于 fork 选择列表/搜索。

**参数**：同 get_messages 的 view/after/before/limit。

**返回**：
```json
{
  "inputs": [{"turnId": 1, "entryId": "msg_001", "text": "帮我重构..."}],
  "hasMore": false, "totalCount": 10, "nextCursor": null
}
```

### 2.5 get_turn_detail（单轮明细）

**用途**：展开某轮看全部 entry（含 tool_call/tool_result/thinking）。**不分页**。

**参数**：`turnId: number`, `include_custom?: string`

**返回**：
```json
{
  "turnId": 3,
  "entries": [{ "entryId", "type", "role", "content", "toolCall", "isError" }],
  "overview": { "userContent", "assistantContent", "keySteps", "toolCallCount", "tokens", "durationMs", "status", "modifiedFiles": ["src/rpc.rs", "src/session_jsonl.rs"] }
}
```

> `overview.modifiedFiles`（P2）：本轮被 edit/bash 工具改动过的文件列表，前端可显示"改了 2 个文件"。

### 2.6 get_tree（会话树/分支地图）

**用途**：返回会话的结构信息，用于渲染分支地图。

**两种模式**：

| mode | 返回 | 适合 |
|------|------|------|
| `"structure"`（默认） | 只返回分支骨架：compaction 点 + leaf_pointer + 分支末端节点 + branches | 渲染分支地图（几十条，极轻量） |
| `"full"` | 返回全部 entry 骨架（id/parentId/type/turnId，不含正文） | 全量审计/重建树（配合分页） |

**参数**：
| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `mode` | string | `"structure"` | structure / full |
| `after`/`before`/`limit` | — | — | full 模式才分页，structure 不分页 |

**返回**：见 §1.3 TreeResponse（structure 模式 nodes 只含结构节点）。

### 2.7 get_session_stats（会话统计）

**用途**：从 index 读，O(1)，不扫消息。

**参数**：无。

**返回**：见 §1.4 SessionMeta（统计部分 + lastEntryId + 血缘字段）。

### 2.8 get_children（查子会话）

**用途**：反向索引 O(1) 查直接子会话。血缘只一层，**不递归**（要看整棵血缘树前端递归调用）。

**参数**：`session: string`

**返回**：
```json
{ "children": [{"id": "sess_branch", "name": "...", "parentEntry": "msg_017", "turnCount": 4, "updatedAt": "..."}] }
```

### 2.9 subscribe（实时事件流）

**用途**：前端接实时流。与 get_messages 用 entryId 去重拼接。

**事件类型**：agent_start / text_delta(delta, entryId) / tool_call(entryId, tool) / agent_end。

**不推**：model_change / compaction / deletion / segment_summary（这些只能在历史拉取里拿到）。

---

## 三、分页规则

### 3.1 游标分页（entryId/turnId）

使用 `{after, before, limit}` 三元组，**不用 offset**。原因：compaction/branch 会让 offset 漂移。

- `after: "msg_050"` → 返回 msg_051 开始（正向，用于断线补齐）
- `before: "msg_050"` → 返回 msg_050 之前的（反向，用于向上滚动加载）
- 游标 = entryId（消息流）或 turnId（概览）
- `limit` 默认 **50**。前台全量需显式 `limit:0`

### 3.2 turn 完整性（complete_turn: true，默认）

分页边界如果切断了一轮 turn，自动多带几条补齐。例如请求 20 条，但第 18-22 条属于同轮，实际返回 22 条。`pageInfo.completedTurnBoundary` 标识补齐边界。

关闭 `complete_turn:false` 时严格按 limit 切（适合纯流式渲染）。

### 3.3 可见性过滤层（默认生效，view:full 绕过）

所有拉取默认经过用户视角过滤器：
1. **回滚过滤**：leaf_pointer 之后的回滚段被排除
2. **删除过滤**：deletion entry 的 targetIds 被排除
3. **折叠替换**：segment_summary 命中的消息替换为 BranchSummary
4. **压缩过滤**：since_compaction 视点排除 compaction 之前的

| 视点 | 经过去滤器? | 能看到回滚段? |
|------|-----------|------------|
| `live` | ✅ | ❌ |
| `since_compaction` | ✅ | ❌ |
| `branch:<leaf>` | ✅（该分支范围） | ✅（指定分支的内容） |
| `full` | ❌ **不过滤** | ✅（全量审计） |

---

## 四、旁路数据 & customType 两维属性

### 4.1 entry 类型分类

| entry type | 进 LLM? | 用途 |
|-----------|---------|------|
| `message` | ✅ | user/assistant/toolCall/toolResult |
| `custom_message` | 由扩展声明 | 扩展消息（可进可不出） |
| `custom` | ❌ | 纯旁路数据（不进模型） |
| `system_event` | ❌ | 系统事件（模型切换/压缩/重试） |
| `compaction` | ❌ | 压缩锚点（消息层面的元标记） |
| `branch_summary` | ✅（替换非追加） | 折叠后的摘要（替换被折叠段） |
| `deletion` | ❌ | 软删除标记（不展示，影响可见性） |
| `segment_summary` | ❌ | 手动折叠标记（替换 targetIds 段） |
| `leaf_pointer` | ❌ | 记录活跃分支指针 |

### 4.2 customType 两维属性（进化 LLM? + 展示?）

| customType | 进 LLM? | 展示? | 用途 |
|-----------|---------|-------|------|
| `model_change` | ❌ | ✅ | 模型切换 UI 提示 |
| `compaction_done` | ❌ | ✅ | 压缩完成通知 |
| `compacting` | ❌ | ❌ | 压缩中（内部状态） |
| `llm_retry` | ❌ | ❌ | LLM 重试（内部状态） |
| `retry_exceeded` | ❌ | ❌ | 重试达上限（内部状态） |
| `process_started` | ❌ | ❌ | 后台进程启动（内部状态） |
| `memory_saved` | ❌ | ✅ | 主动保存记忆 |
| `memory_search` | ❌ | ❌ | 自动检索（背后执行） |
| `memory_injected` | ✅ | ❌ | 记忆注入（模型看到、用户不看到） |

插件 customType 由扩展自行声明，不占用内核命名空间。

### 4.3 include_custom 参数

| 值 | 行为 |
|----|------|
| `"none"`（默认） | 只拉 message 类型 |
| `"display_only"` | 带 display:true 的旁路 |
| `"all"` | 全部（含 display:false 的隐藏事件） |

> deletion/segment_summary 的影响不受 include_custom 控制——它们默认就改变可见性（msg 被删就看不到、被折叠就替换为摘要），即使 include_custom:"none"。

---

## 五、数据架构：3 层流动

### Layer 1: In-Flight（实时事件流，未定型）

**机制**：subscribe 收到 text_delta，前端按 entryId 聚合成整条消息。

**聚合规则**：共享同一 entryId 的所有 text_delta → 合并为一条完整 content。agent_end 前消息不定型。

**不可查询**：get_messages 拉不到 L1 的数据。

**只推不拉的信息**：text_delta、tool_call（实时）、agent_start、agent_end。

### Layer 2: Hot Memory（已定型，未落盘）

**机制**：agent_end → 消息定型，追加到 `agent.messages()`。

**可查询**：get_messages 读到 L2（热段）+ L3（冷段）的线性叠加。

**叠加公式**：
```
get_messages 返回 = L3(磁盘冷段) + L2(内存热段)，按 entryId 排序
```

### Layer 3: Cold Disk（已持久化）

**机制**：增量 append（saved_msg_count 偏移），不覆盖旧行。

**可查询**：持久化后可永久查询。entryId 与 L2 一致。

### 拼接规则（前端关键）

```
历史拉取(msg_001~msg_019) → nextCursor: "msg_019" → 最后一条 entryId
实时订阅(text_delta, entryId: msg_020) → 从 msg_020 开始，无重复
```

**断线重连**：用 `after:"msg_019"` 补齐断线期间的消息。

**"实时不推"清单**：model_change / compaction / deletion / segment_summary 只在历史拉取里有。推荐策略：agent_end 后触发一次 `get_messages` 补。

---

## 六、UI 渲染规范（6 种风格）

### 6.1 消息流（完整时间线）

**特征**：
- 从第一条到最后一条全可见（分页时逐页加载）
- 压缩内容默认折叠（黄色虚线框 + `▶` 展开），展开可看压缩摘要
- 右侧导航栏（Turn/User 双模式切换 + 分页 + ID 跳转输入框）
- 置顶按钮（滚动 400px 后出现）
- 每轮 turn 显示在消息头部（msg-turn-badge）

**交互**：
- 点击导航项 → 滚动到对应位置（高亮）
- 点击 entryId（`msg_014`）→ 填充 ID 跳转输入框 → 跳转 + 加载前后 N 条
- 向上滚动 → `load-more-bar` 加载更早

**导航栏**：
- Turn 模式：列出所有 turn（含压缩点、abort 标记）
- User 模式：只列出用户提问消息
- 底部导航分页（`‹ 1/3 ›`）
- 支持按 entryId 快速定位（`跳转 · 加载前后 10 条`）

### 6.2 User 维度折叠（Codex 风格）

**特征**：
- 每个 User 提问一个**独立块**（蓝色左边框）
- Agent 回复（agent_start → agent_end 区间）**默认折叠**，只显示：
  - 状态图标（✓ / ✗ / ⚠）
  - 预览文本（最终结果）
  - token 数
- 点击展开看 Agent 执行过程（tool_call + tool_result + 中间输出）

**状态指示**：
- ▸ 折叠态 → ▾ 展开态（箭头旋转）
- 完成的 ✓ 绿色，失败的 ✗ 红色，中断的 ⚠ 红色虚线
- 运行中的显示闪烁加载点

### 6.3 Agent 执行时间线（Dark LLM Panel 风格）

**特征**：
- 纯黑背景（#111），无卡片无边框
- 左侧一列圆点时间线（暗灰 #242424 / 当前稍亮 #8A8A8A）
- 大字号（20px）宽松排版（行高 1.8）
- 代码/函数/路径名使用深灰圆角药丸（#2A2A2D）

**布局**：
- 顶部弱化标题「思考过程 ›」(#888)
- 每段日志之间 32px 垂直间距
- 状态行（灰色 #6F6F6F 15px）→ 正文（浅灰 #E5E5E5 20px）
- 方法标签（`[get_messages]`）在状态行右侧，鼠标悬停变亮

**色彩语义**：
- 用户提问：稍大 22px 加粗
- 工具调用成功：绿色
- 工具调用失败：红色
- 压缩点：黄色
- 内部状态：灰斜体

### 6.4 移动端折叠（APP 风格）

**特征**：
- 窄宽度（max 480px）
- 每个 User 提问一个锚点（U 头像 + 正文 + 时间）
- Agent 块默认折叠（紧凑，状态行 + 预览 + token）
- 运行时块展开（闪烁加载点 + 实时 delta）

**交互**：
- 点击折叠/展开 Agent 块（tap 友好，`-webkit-tap-highlight-color: transparent`）
- 底部固定工具栏（显示当前使用的接口名）
- abort 态不可展开（无过程可看，只显示中断标记）

### 6.5 统计面板

**特征**：
- 8 个指标卡（网格布局）
- Token 柱状图显示每轮分布（哪轮最贵一目了然）
- 错误/abort 用红色高亮

**指标**：总轮数 / 消息数 / Token(in+out+cache) / 花费 / 时长 / 错误数 / 压缩次数

### 6.6 关系图谱（编排拓扑 / 会话血缘）

**编排拓扑**（Worker 关系）：
- 主会话（蓝色）→ spawn_worker（线）→ 子 Worker（绿色）
- Fork 出去的会话（紫色虚线）
- Channel 通信链路（青色标记 + 内容）
- 并行框（橙色虚线框）

**会话血缘**（跨文件）：
- 根会话（蓝色左边框，parentSession: null）
- Fork 出的子会话（黄色虚线边框）
- spawn 出的子 Worker 会话（紫色边框）
- 每条卡片显示 parentSession/parentEntry/parentType
- 连接线标注 fork @ 某 entry / spawn 关系

---

## 七、优先级

| 级别 | 风格 | 适用场景 |
|------|------|---------|
| P0 | 消息流（时间线） | 桌面端默认视图，全量加载+分页 |
| P0 | User 折叠（Codex） | 桌面端紧凑视图，Agent 过程折叠 |
| P1 | Agent 时间线（Dark LLM） | 开发者调试/运维面板 |
| P1 | 移动端 APP | 手机端，性能优先 |
| P1 | 统计面板 | 会话成本/健康度查看 |
| P2 | 关系图谱 | 编排调优/血缘追溯 |
| P2 | 删除折叠对比 | 审计排查 |
| P2 | 用户输入列表 | Fork / 搜索选择器 |

---

## 八、关键设计决策索引

| 决策 | 选择 | 原因 |
|------|------|------|
| 分页方式 | 游标（after/before） | compaction/branch 让 offset 漂移 |
| 默认分页 | limit=50（不是全量） | >100MB 大会话安全默认 |
| turn 完整性 | complete_turn=true（默认） | 前端 UI 渲染不切半截 turn |
| **abort/error turn** | **强制有 turn_summary** | 中断的 turn 不能从 list_turns 漏掉 |
| **list_turns content** | **默认截断 200 字** | 用户贴大文件不撑爆响应；full_content:true 取全 |
| 血缘模型 | 只一层直接 parent | 不传递祖先链，fork 是独立会话 |
| copy vs fork | copy 消解进 fork | fork 不填 parentEntry = copy 当前上下文 |
| **get_children** | **只直接子，不递归** | recursive 由前端递归调用实现 |
| customType 定义 | 内核不独占命名空间 | 插件扩展自己声明 |
| 旁路过滤 | 两维正交（进LLM? + 展示?） | 4 种组合覆盖所有场景 |
| 可见性过滤 | 默认生效，view:full 绕过 | 用户自然顺序，审计全量可选 |
| 历史 vs 实时 | entryId 去重拼接 | 同一 ID 体系，安全合并 |
| summary | 实时算，不预存 | list_turns 带真实 content |
| **get_tree 双模式** | **structure(默认)/full** | 分支地图只几十条；全量审计才分页 |
| **SessionMeta.lastEntryId** | **增量拉取锚点** | 前端存上次看到的 entryId，对比判断新消息 |
| **list_sessions 规格** | **独立接口，带血缘** | 进入拉取的入口，不能假设用户已知 sessionId |

---

## 八点五、性能优化记录（2026-07-10）

### 复杂度对照（ION vs pi）

| 操作 | ION | pi | 优势方 |
|------|-----|----|-------|
| list_sessions | O(m) 读单 index.json | O(ΣFᵢ) 逐个全量读所有 jsonl | **ION 优** |
| get_children（跨会话）| O(n) 扫 HashMap + 读 index | O(S) 扫已加载 session（前置 list 全盘读） | **ION 优** |
| 全文搜索 | ❌ 未实现 | O(S×L) 线性匹配（无倒排）| pi 有但慢 |
| SessionFile 加载 | **缓存命中 O(1)**，未命中 O(L) | 一次 load 进 byId Map 后内存复用 | **ION 优**（优化后）|
| get_messages 分页 | 整盘读+O(n) 过滤+**缓存命中跳过读盘** | 内存复用 + O(d) 重建 + slice | 持平 |
| resolve_current_leaf | **O(n) 单趟 DP**（优化后）| 无对应（byId Map 直接查）| 持平 |
| get_branch_path | O(depth) 回溯 + O(n) 建表 | O(depth) byId 已驻留 | pi 略优 |

### 已实施的优化

| # | 优化 | 前 | 后 | 文件 |
|---|------|----|----|------|
| 1 | **SessionFile mtime 缓存** | 每次 RPC 整盘读+逐行解析 O(L) | mtime 未变时 O(1) 缓存命中 | `message_retrieval.rs` load_entries_cached |
| 2 | **resolve_current_leaf O(n²)→O(n)** | 逐个 entry_depth 嵌套回溯 O(n×depth) | compute_depths 单趟拓扑 DP O(n) | `session_tree.rs` |

### 待优化（P2 后续）

| # | 优化 | 当前 | 目标 |
|---|------|------|------|
| 3 | retrieve_messages 短路分页 | limit 小时仍全量过滤 | limit 小时只取最后 N 条过滤 |
| 4 | get_branch_path 复用 HashMap | resolve+branch 建两次表 | 共享一次建表 |
| 5 | SessionIndex 内存缓存 | 每次 load 整盘读 | 进程内驻留，save 时更新 |
| 6 | get_children 反向索引 | O(n) 扫 HashMap | parentId→children Map，O(1) |

---

## 九、定稿确认（2026-07-09）

本设计经系统审查确认以下 8 个调整已落实：

1. ✅ get_tree 双模式（structure 轻量 / full 分页）
2. ✅ list_turns content 默认截断 200 字 + full_content 参数
3. ✅ abort/error turn 强制有 turn_summary
4. ✅ list_inputs 分页结构统一（加 nextCursor）
5. ✅ get_children 去掉 recursive
6. ✅ SessionMeta 加 lastEntryId
7. ✅ list_sessions 正式接口规格
8. ✅ get_turn_detail overview 加 modifiedFiles（P2）

接口总数：**9 个**（list_sessions / get_messages / list_turns / list_inputs / get_turn_detail / get_tree / get_session_stats / get_children / subscribe）。设计**定稿**，可进入实现阶段。
