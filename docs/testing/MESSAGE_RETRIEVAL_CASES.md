# 会话消息拉取 — CLI 用例集

> **状态:设计定稿(2026-07-09)** — 经系统审查确认 8 个调整已落实,9 个接口定稿,可进入实现阶段。
>
> 基于 `get_messages` / `list_turns` / `list_inputs` / `get_turn_detail` / `get_tree` 五个核心拉取接口,加 `list_sessions` / `get_session_stats` / `subscribe` / `get_children` 四个配套接口的 CLI 验证用例(共 12 个 Group A-L)。
>
> 本文档只列**用户场景 + CLI 命令 + 预期结果**,不含 Rust 实现。实现细节见 `/tmp/ion_demo/DESIGN_SPEC.md`。

---

## 接口速查

> **设计定稿(2026-07-09)**:9 个接口,经系统审查确认。详见 DESIGN_SPEC.md。

**核心拉取接口(5 个)**:

| 接口 | 粒度 | 分页 | 用途 |
|------|------|------|------|
| `get_messages` | 全消息 | ✅ entryId 游标 | 拉消息流(消息列表),含压缩前 |
| `list_turns` | turn 概览 | ✅ turnId 游标 | 逐轮预览(content 默认截断 200 字,full_content:true 取全) |
| `list_inputs` | user 输入 | ✅ entryId 游标 | 快速拉用户提问 |
| `get_turn_detail` | 单 turn 明细 | ❌ 不分页 | 展开某轮看全部细节(overview 含 modifiedFiles) |
| `get_tree` | 树拓扑 | structure 不分页 / full 分页 | 双模式:分支骨架(轻量) / 全部 entry 骨架 |

**配套接口(4 个)**:

| 接口 | 用途 | 出现在 |
|------|------|--------|
| `list_sessions` | 会话列表(入口,含血缘 parentSession + lastEntryId) | Group F0, L |
| `get_session_stats` | 会话级统计 + 血缘 + lastEntryId(从 index 读) | Group K1, L1 |
| `get_children` | 查直接子会话(反向索引 O(1),**只一层不递归**) | Group L2 |
| `subscribe` | 实时事件流(push,与历史拉取拼接) | Group I |

**公共参数(所有接口可选)**:

| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `view` | string | `"live"` | 视点:`live` / `since_compaction` / `branch:<leaf_id>` / `full` |
| `after` | string | — | 正向分页游标(entryId 或 turnId),向后翻页 |
| `before` | string | — | 反向分页游标,向上滚动加载历史 |
| `limit` | number | `50` | 每页条数。`0` = 全量(显式 opt-out 分页) |
| `complete_turn` | bool | `true` | 分页时保证每轮 turn 完整(边界不切断 turn) |
| `include_custom` | string | `"none"` | 旁路数据:见下方"分页规则" |
| `full_content` | bool | `false` | list_turns 专用:content 默认截断 200 字,true 取全 |

### 分页规则

**1. 默认分页,不是默认全量**

数据拉取默认进入分页(`limit=50`)。全量拉取需要显式传 `limit: 0` 或调大 limit。原因:大会话(>100MB)默认全量会撑爆内存,分页是安全默认值。

**2. turn 完整性保证(`complete_turn`,默认开启)**

分页边界如果切断了一轮 turn(比如 limit=20 时第 18-22 条属于同一轮),自动多带几条把这一轮凑齐,不返回半截 turn。

```
请求: get_messages { limit: 20 }
                    ↓ complete_turn: true(默认)
                    ↓
原始 20 条:  msg_001..msg_020
其中 msg_018~022 属于 turn 3
                    ↓ 自动补齐
实际返回:    msg_001..msg_022  (22 条,turn 3 完整)
nextCursor:  "msg_022"
```

关闭 `complete_turn: false` 时严格按 limit 切(可能返回半截 turn,适合纯流式渲染不关心 turn 边界的场景)。

**3. 可见性过滤层(用户视角,默认生效)**

所有拉取默认经过一套**自上而下的可见性过滤器**,让结果符合"用户自然顺序"——回滚吐出的段、被删除的、被压缩的死数据,默认都看不到。只有专门的视点/接口能绕过。

```
原始 JSONL 文件(append-only,所有 entry 都在)
    │
    ▼  ┌──────────── 可见性过滤器(默认对 live/since_compaction/branch 生效)───────────┐
    │  │ 1. 回滚过滤:leaf_pointer 之后的"回滚吐出段"被排除(用户视角看不到)         │
    │  │ 2. 删除过滤:deletion entry 的 targetIds 被排除                            │
    │  │ 3. 折叠替换:segment_summary 命中的消息替换为一条 BranchSummary            │
    │  │ 4. 压缩过滤:compaction 之前的死数据,在 since_compaction 视点被排除       │
    │  └──────────────────────────────────────────────────────────────────────────┘
    │
    ▼  过滤后的"用户视角"消息
    │
    ▼  再按 view / 粒度 / 分页 切分
```

| 视点 | 经经过滤器? | 能看到回滚吐出段? |
|------|-----------|------------------|
| `live`(默认) | ✅ 过滤 | ❌(用户视角:回滚后看不到回滚前的) |
| `since_compaction` | ✅ 过滤 | ❌ |
| `branch:<leaf>` | ✅ 过滤(该分支范围内) | ✅(能看到指定被回滚分支的内容) |
| `full` | ❌ **不过滤** | ✅(全量审计,原始顺序,含所有分支所有 entry) |

**关键**:回滚在 JSONL 里是 append 一条 `leaf_pointer`,**原始数据不删**。但从用户角度,回滚后那段"拉不出来"——这正是过滤层的作用。想强行看回滚吐出的数据,用:
- `view:"full"`(全量,不过滤)
- `view:"branch:<回滚前leaf>"`(指定看那个分支)
- `get_tree`(看分支地图,知道哪里被回滚了)

> 文件内回滚(Group E 的 leaf_pointer)和跨文件 fork(Group L 的 parentSession)都走这套过滤:回滚 → 默认过滤;fork → 数据物理在新文件,不存在过滤问题。

**4. 旁路数据(`include_custom`)**

会话中除正常 user/assistant 消息外,还有旁路 entry。它们各有"**进 LLM?/展示?**"两维属性(见 G5 详细分类):

| entry type | 用途 | 例子 |
|------------|------|------|
| `custom` | 纯旁路数据(不进 LLM) | 扩展保存的学习记录、记忆搜索过程 |
| `custom_message` | 扩展消息(进 LLM 或不进,由扩展声明) | 记忆注入、系统提示 |
| `system_event` | 系统事件(不进 LLM) | 压缩中、重试次数过多、模型切换 |

`include_custom` 控制是否带这些数据:

| 值 | 行为 |
|----|------|
| `"none"`(默认) | 只拉 `message` 类型(进 LLM 的数据) |
| `"display_only"` | 带 `display:true` 的旁路 entry(前端可展示的) |
| `"all"` | 带所有旁路 entry(含 `display:false` 的隐藏事件,如重试/压缩内部状态) |

---

## Group A:全量拉取(数据量少/中)

> **场景**:普通会话,数据量不大,**显式 opt-out 分页**一次性全量拉取含压缩前的所有历史。覆盖前述 case A(全量消息)和 A'(turn 概览)。

### A1 全量拉取所有消息

**场景**:刚结束一段对话,想导出/审计全部聊天记录。注意:**默认是分页的**,全量需显式传 `limit:0`。

```bash
# 先建一个会话聊几句
ion rpc --session sess_demo --method prompt --params '{"text":"帮我列出项目结构"}'
ion rpc --session sess_demo --method prompt --params '{"text":"再加一个测试目录"}'

# 全量拉取(显式 limit:0 = 不要分页)
ion rpc --session sess_demo --method get_messages --params '{"limit": 0}'
```

**预期响应**:

```json
{
  "type": "response",
  "id": "1",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_001", "role": "user", "content": "帮我列出项目结构" },
      { "entryId": "msg_002", "role": "assistant", "content": "..." },
      { "entryId": "msg_003", "role": "user", "content": "再加一个测试目录" },
      { "entryId": "msg_004", "role": "assistant", "content": "..." }
    ],
    "hasMore": false,
    "totalCount": 4,
    "nextCursor": null,
    "view": "live",
    "compactionPoints": []
  }
}
```

**验证点**:
- ✅ `messages` 包含全部 4 条消息
- ✅ `hasMore: false` / `nextCursor: null`(全量无分页)
- ✅ 每条消息带 `entryId`(后续分页/跳转用)
- ✅ `view: "live"` 回显实际生效的视点

### A2 全量拉取 turn 概览(逐轮卡片视图)

**场景**:前端想渲染"每轮对话一张卡片"。每轮带**真实的用户/回复内容**(有就带,没有就没有)+ 元信息 + 方法(工具调用序列)。summary 是**实时算的可选项**,不是必带、也不是预存的。

```bash
# 全量(显式 limit:0)
ion rpc --session sess_demo --method list_turns --params '{"limit": 0}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "list_turns",
  "success": true,
  "data": {
    "turns": [
      {
        "turnId": 1,
        "userId": "msg_001",
        "userContent": "帮我列出项目结构",
        "assistantId": "msg_002",
        "assistantContent": "好的,项目结构如下:\n- src/\n- tests/\n- docs/",
        "keySteps": [{"tool": "ls", "path": "."}, {"tool": "read", "path": "README.md"}],
        "toolCallCount": 2,
        "tokens": { "input": 150, "output": 200 },
        "durationMs": 1200,
        "entryCount": 2,
        "status": "completed"
      },
      {
        "turnId": 2,
        "userId": "msg_003",
        "userContent": "再加一个测试目录",
        "assistantId": "msg_004",
        "assistantContent": "已创建 tests/ 目录",
        "keySteps": [{"tool": "bash", "command": "mkdir tests"}],
        "toolCallCount": 1,
        "tokens": { "input": 180, "output": 90 },
        "durationMs": 800,
        "entryCount": 2,
        "status": "completed"
      }
    ],
    "hasMore": false,
    "totalCount": 2,
    "nextCursor": null,
    "view": "live"
  }
}
```

**字段说明**:

| 字段 | 来源 | 说明 |
|------|------|------|
| `userContent` | 从 user message 直接取 | 用户问了什么(**真实内容**,有就带,没有就空) |
| `assistantContent` | 从 assistant message 直接取 | 回复了什么(**真实内容**) |
| `keySteps` | 从 tool_call 提取 | 方法/工具调用序列(结构化,前端渲染成步骤) |
| 元信息 | 累加 | turnId / tokens / duration / entryCount / status |
| `summary` | **实时算(可选)** | 不是必带,也不是预存的。请求时带 `summarize:true` 才现场生成 |

**验证点**:
- ✅ `userContent`/`assistantContent` 是**真实消息正文**(直接取,不造摘要)
- ✅ 有 content 就带,没有就没有(不强行总结)
- ✅ `keySteps` 是结构化的工具调用(工具名+参数),不是文字摘要
- ✅ summary 默认**不返回**;要的话传 `summarize:true` 实时算
- ✅ 元信息(tokens/duration/status)方便前端卡片渲染

### A3 全量拉取用户输入(快速)

**场景**:想做 fork 选择列表 / 搜索历史提问,只要用户说的话。

```bash
# 全量(显式 limit:0)
ion rpc --session sess_demo --method list_inputs --params '{"limit": 0}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "list_inputs",
  "success": true,
  "data": {
    "inputs": [
      { "turnId": 1, "entryId": "msg_001", "text": "帮我列出项目结构" },
      { "turnId": 2, "entryId": "msg_003", "text": "再加一个测试目录" }
    ],
    "hasMore": false,
    "totalCount": 2,
    "view": "live"
  }
}
```

**验证点**:
- ✅ 只返回 user 消息(过滤掉 assistant/tool)
- ✅ 每条只 3 个字段(turnId/entryId/text),极轻量
- ✅ 数据量可控时默认全量

---

## Group B:大数据量分页拉取

> **场景**:长会话(>100MB 或几百轮),不能一次性全拉。覆盖前述 case B1(先拉最新)和 B2(向上滚加载/导航跳转)。

### B1 先拉最新一批消息(反向首屏)

**场景**:会话已经很长,前端打开时先显示最新的 N 条,向上滚动再加载更早的。**默认就是分页的**(limit=50),所以不传 limit 也是分页。

```bash
# 首屏拉最新一批(默认 limit=50,含压缩前)
ion rpc --session sess_big --method get_messages

# 或者显式指定条数
ion rpc --session sess_big --method get_messages --params '{"limit": 20}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* 最新 20 条 */ ],
    "hasMore": true,
    "totalCount": 850,
    "nextCursor": "msg_830",
    "view": "live",
    "compactionPoints": [
      { "entryId": "cmp_005", "summary": "...", "tokensBefore": 32000 }
    ]
  }
}
```

**验证点**:
- ✅ 只返回最新 20 条(不是从头 20 条)
- ✅ `hasMore: true` 表示还有更早的
- ✅ `nextCursor` 给出向上加载的起点
- ✅ `compactionPoints` 旁路返回压缩锚点(前端可标记"以下是压缩后的")

### B2 向上滚动加载更早的消息

**场景**:用户滚到顶部,前端用 `nextCursor` 拉更早的一批。

```bash
# 用上一页返回的 nextCursor 作为 before 游标(默认 limit=50)
ion rpc --session sess_big --method get_messages \
  --params '{"before": "msg_830"}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* msg_780 ~ msg_829(50 条,complete_turn 可能多带几条) */ ],
    "hasMore": true,
    "totalCount": 850,
    "nextCursor": "msg_779",
    "view": "live"
  }
}
```

**验证点**:
- ✅ 返回 `msg_830` 之前的消息(不含 msg_830 本身)
- ✅ `nextCursor` 继续往前推
- ✅ `complete_turn: true` 保证返回的最后一条是某轮 turn 的结尾,不切半截 turn
- ✅ 可连续调用实现无限滚动

### B3 导航跳转到指定位置

**场景**:用户想直接跳到会话中段(比如从 `list_turns` 看到第 50 轮有意思),不等滚动。

```bash
# 从 turn 摘要拿到第 50 轮的 entryId,直接跳过去拉前后各 20 条
ion rpc --session sess_big --method get_messages \
  --params '{"before": "msg_420", "limit": 20}'
```

**验证点**:
- ✅ `before` 游标支持任意 entryId 跳转,不限于上一页的 nextCursor
- ✅ 配合 `list_turns` 的 entryId 实现"卡片点击→跳到对应消息"

### B4 大数据量 turn 概览分页

**场景**:300 轮的长会话,前端列表不能一次渲染所有卡片,分页拉。

```bash
# 先拉最新 30 轮的卡片
ion rpc --session sess_big --method list_turns \
  --params '{"limit": 30}'

# 向上加载更早的轮次
ion rpc --session sess_big --method list_turns \
  --params '{"before": "270", "limit": 30}'
```

**验证点**:
- ✅ turn 摘要也支持 `before`/`limit` 反向分页
- ✅ 游标用 turnId(字符串形式),与消息分页的 entryId 区分
- ✅ 每页 30 轮摘要,数据量远小于拉消息正文

---

## Group C:逐轮渲染 + 按需展开

> **场景**:前端以"一轮对话"为单位渲染,默认显示概览,点击展开才拉这一轮的完整内容。覆盖前述 case C。

### C1 默认渲染 turn 列表(概览)

```bash
ion rpc --session sess_demo --method list_turns \
  --params '{"limit": 50}'
```

前端拿到 `turns` 数组,每条渲染成一张卡片:标题=`userContent`,正文=`assistantContent`,步骤=`keySteps`,时间=`durationMs`。

### C2 点击展开某轮拉完整详情

**场景**:用户点开第 3 轮卡片,前端拉这一轮的全部 entry(用户消息、assistant 文本、tool_call、tool_result、thinking)。

```bash
# 用 turnId 拉单轮明细(不分页,一轮数据量可控)
ion rpc --session sess_demo --method get_turn_detail \
  --params '{"turnId": 3}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_turn_detail",
  "success": true,
  "data": {
    "turnId": 3,
    "entries": [
      { "entryId": "msg_007", "type": "message", "role": "user", "content": "帮我重构消息拉取接口" },
      { "entryId": "msg_008", "type": "message", "role": "assistant", "content": "好的,我先读一下..." },
      { "entryId": "msg_009", "type": "message", "role": "assistant", "toolCall": { "tool": "read", "path": "src/rpc.rs" } },
      { "entryId": "msg_010", "type": "message", "role": "toolResult", "content": "..." },
      { "entryId": "msg_011", "type": "message", "role": "assistant", "content": "重构完成" }
    ],
    "overview": {
      "userContent": "帮我重构消息拉取接口",
      "assistantContent": "重构完成",
      "keySteps": [{"tool": "read", "path": "src/rpc.rs"}, {"tool": "edit", "path": "src/rpc.rs"}, {"tool": "bash", "command": "cargo test"}],
      "toolCallCount": 3,
      "tokens": { "input": 1200, "output": 800 },
      "durationMs": 5200,
      "status": "completed"
    }
  }
}
```

**验证点**:
- ✅ `entries` 包含该轮所有 entry(完整内容,不省略)
- ✅ 不分页(单 turn 数据量有上限)
- ✅ 同时返回 `overview`(与 list_turns 的字段一致:userContent/assistantContent/keySteps/元信息),前端可直接复用
- ✅ 含 tool_call / tool_result / thinking 等所有类型

### C3 轮内导航:从详情跳回消息流

**场景**:在 turn 详情里看到某条 tool_result,想看它在整个消息流里的上下文。

```bash
# 从 get_turn_detail 拿到 entryId,跳到消息流对应位置
ion rpc --session sess_demo --method get_messages \
  --params '{"before": "msg_010", "limit": 10}'
```

**验证点**:
- ✅ entryId 在 turn 详情和消息流之间通用,可双向跳转

---

## Group D:含压缩历史的拉取

> **场景**:会话压缩过多次,验证"默认含压缩前数据" + "可选只看压缩后"。

### D1 默认拉取包含压缩前的数据

**场景**:会话压缩过,但用户要看完整历史(前端默认视图)。

```bash
# 假设会话触发过压缩(message_count 超阈值)
ion rpc --session sess_compacted --method get_messages

# view 不传 = live = 含全部历史(含压缩前)
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* 压缩前的旧消息 + 压缩后的新消息,全都在 */ ],
    "hasMore": false,
    "totalCount": 150,
    "nextCursor": null,
    "view": "live",
    "compactionPoints": [
      { "entryId": "cmp_003", "summary": "前 50 轮的摘要", "tokensBefore": 32000 }
    ]
  }
}
```

**验证点**:
- ✅ `totalCount` 包含压缩前的消息(不是只剩压缩后的)
- ✅ `compactionPoints` 标出哪里压缩过,前端可插入分隔线"↑ 以下是压缩前的历史"
- ✅ 默认行为不变(向后兼容)

### D2 只拉压缩后的部分(opt-in)

**场景**:调试 LLM 上下文——只想看模型当前实际看到的(压缩点之后的)。

```bash
# 显式指定 since_compaction 视点
ion rpc --session sess_compacted --method get_messages \
  --params '{"view": "since_compaction"}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* 只有压缩点之后的 */ ],
    "hasMore": false,
    "totalCount": 50,
    "view": "since_compaction",
    "compactionPoints": [
      { "entryId": "cmp_003", "summary": "前 50 轮的摘要", "tokensBefore": 32000 }
    ]
  }
}
```

**验证点**:
- ✅ `totalCount` 明显变小(只剩压缩后)
- ✅ `view` 回显为 `since_compaction`
- ✅ 这是 opt-in,不影响默认全量行为

### D3 多次压缩后拉取

**场景**:会话压缩过 3 次,验证 `since_compaction` 只截到最后一次。

```bash
# 第一次压缩在 cmp_003,第三次在 cmp_010
ion rpc --session sess_multi_compact --method get_messages \
  --params '{"view": "since_compaction"}'
```

**验证点**:
- ✅ `since_compaction` 只保留最后一个 compaction 点(cmp_010)之后的消息
- ✅ 想看中间某次压缩后的历史,用 `view: "full"` + `get_tree` 定位

---

## Group E:会话树与分支

> **场景**:会话发生过回滚/分支,验证分支地图 + 指定分支拉取。

### E1 拉取整棵会话树

**场景**:前端想渲染分支可视化地图,看有哪些分支、当前活的是哪条。

```bash
ion rpc --session sess_branched --method get_tree
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_tree",
  "success": true,
  "data": {
    "tree": {
      "nodes": [
        { "id": "msg_001", "parentId": null,     "type": "message",    "turnId": 1 },
        { "id": "msg_020", "parentId": "msg_019","type": "message",    "turnId": 5 },
        { "id": "msg_035", "parentId": "msg_034","type": "message",    "turnId": 8 },
        { "id": "cmp_010", "parentId": "msg_009","type": "compaction", "label": "压缩点·32k tokens" },
        { "id": "lp_001", "parentId": null,      "type": "leaf_pointer","leafId": "msg_042" }
      ],
      "currentLeaf": "msg_042",
      "branches": [
        { "id": "msg_035", "label": "回滚前分支", "turnRange": [5, 8], "abandoned": true },
        { "id": "msg_042", "label": "活跃分支",   "turnRange": [5, 10], "active": true }
      ],
      "compactionPoints": ["cmp_010"]
    }
  }
}
```

**验证点**:
- ✅ 只返回骨架(id/parentId/type),不含消息正文(轻量)
- ✅ `currentLeaf` 指出当前活跃分支末端
- ✅ `branches` 标出每条分支的起止 turn + 是否被废弃
- ✅ `compactionPoints` 单独列出压缩点

### E2 拉取指定被回滚的分支

**场景**:用户想回看被回滚掉的分支(msg_035 那条)的内容。

```bash
# 从 get_tree 拿到废弃分支的 leaf id,用它拉消息
ion rpc --session sess_branched --method get_messages \
  --params '{"view": "branch:msg_035"}'
```

**验证点**:
- ✅ 只返回 root→msg_035 路径上的消息
- ✅ 不包含回滚后新分支(msg_042 那条)的消息
- ✅ 被回滚的数据物理保留在文件里,可完整拉取

### E3 live 视点自动跟随回滚

**场景**:回滚操作发生后,默认 `live` 视点应该自动指向回滚后的分支。

```bash
# 假设刚才执行了回滚到 msg_035
# 默认 view=live 应该看到回滚后的分支
ion rpc --session sess_branched --method get_messages

# 应返回 root→msg_035 的消息,而不是回滚前到 msg_042 的
```

**验证点**:
- ✅ `view: "live"` 自动解析到回滚后的分支
- ✅ 前端无需特殊处理,回滚后默认视图就对了

### E4 全量视点看所有分支

**场景**:审计/导出,要看整个文件的所有 entry(含所有分支、含被回滚的)。

```bash
ion rpc --session sess_branched --method get_messages \
  --params '{"view": "full"}'
```

**验证点**:
- ✅ 返回所有 entry(多条分支都在)
- ✅ 前端可用 parentId 重建树,配合 get_tree 渲染
- ✅ 数据量可能较大,可配合分页

---

## Group F:组合场景(真实工作流)

> **场景**:模拟真实前端交互,多个接口配合使用。

### F0 从会话列表进入拉取(入口衔接)

**场景**:用户记不住 session id,先看会话列表、按首条消息/更新时间筛选,再进去拉消息。这是进入所有拉取的前置步骤——没有这步,前面的 Group 都是"空中楼阁"。

```bash
# 1. 列出所有会话(从 sessions.index.json,轻量,不含正文)
ion rpc --method list_sessions

# 预期返回 SessionMeta 列表(每条含 id/name/firstMessage/血缘元信息)
```

**预期响应**:

```json
{
  "type": "response",
  "command": "list_sessions",
  "success": true,
  "data": {
    "sessions": [
      {
        "id": "sess_abc",
        "name": "重构消息拉取接口",
        "firstMessage": "帮我重构 session 模块",
        "model": "glm-4.6",
        "messageCount": 42,
        "turnCount": 8,
        "updatedAt": "2026-07-08T10:00:00Z",
        "project": "/Users/xuyingzhou/Project/study-rust/ion",
        "parentSession": null,
        "parentType": null,
        "parentEntry": null,
        "hasChildren": true,
        "childCount": 1
      },
      {
        "id": "sess_def",
        "name": "重构消息拉取接口(分支2)",
        "firstMessage": "换个思路重构",
        "model": "glm-4.6",
        "messageCount": 12,
        "turnCount": 3,
        "updatedAt": "2026-07-08T11:00:00Z",
        "project": "/Users/xuyingzhou/Project/study-rust/ion",
        "parentSession": "sess_abc",
        "parentType": "fork",
        "parentEntry": "msg_035",
        "hasChildren": false,
        "childCount": 0
      }
    ]
  }
}
```

**血缘元信息字段说明**:

| 字段 | 类型 | 含义 |
|------|------|------|
| `parentSession` | string\|null | 源会话 id。`null` = 根会话(从零开始),非空 = fork 来源 |
| `parentType` | string\|null | `"fork"`(派生)。`null` = 根会话。copy/fork-from-leaf 都是 fork 语义 |
| `parentEntry` | string\|null | 从源会话的哪条 entry 裂变。不填 = fork 当前全部有效上下文(= copy 语义) |
| `hasChildren` | bool | 是否有子会话(向下看血缘) |
| `childCount` | number | 直接子会话数量 |

> **设计定位**:血缘字段是**底层存储时顺带记的元信息**,不是拉取的核心。fork 的完整实现(溯源 compaction、复制有效上下文)是独立设计(见 `docs/design/SESSION_TREE.md`)。拉取文档只关心:加载历史时能知道"这数据从哪来、有没有人继承"。血缘**只记一层直接 parent**,不传递祖先链。

**验证点**:
- ✅ 不读消息正文,只读 index(`sessions.index.json`),极快
- ✅ 每条带 `firstMessage`(用户提问预览,便于列表筛选)
- ✅ 带血缘元信息:fork 出来的会话标 `parentSession`/`parentType:"fork"`,根会话这三个字段为 `null`
- ✅ `hasChildren`/`childCount` 让前端知道哪些会话可展开看子会话
- ✅ 用户选定后,后续 Group 的流程从这里拿 `sess_xxx`

### F1 新会话首次打开的标准流程

```bash
# (前置:F0 拿到 sess_abc)

# 1. 拉会话树(渲染分支地图,极轻量)
ion rpc --session sess_abc --method get_tree

# 2. 拉 turn 概览(渲染卡片列表,默认最新 50 轮)
ion rpc --session sess_xxx --method list_turns \
  --params '{"limit": 50}'

# 3. 用户点击第 3 轮卡片,拉详情
ion rpc --session sess_xxx --method get_turn_detail \
  --params '{"turnId": 3}'

# 4. 用户想看消息流视图,全量拉
ion rpc --session sess_xxx --method get_messages
```

### F2 大会话渐进式加载

```bash
# 1. 首屏:get_tree(分支骨架)+ 最新 20 条消息
ion rpc --session sess_huge --method get_tree
ion rpc --session sess_huge --method get_messages --params '{"limit": 20}'

# 2. 用户切换到"逐轮视图":拉最新 30 轮卡片
ion rpc --session sess_huge --method list_turns --params '{"limit": 30}'

# 3. 用户向上滚:加载更早的 30 轮
ion rpc --session sess_huge --method list_turns --params '{"before": "270", "limit": 30}'

# 4. 用户点开第 100 轮卡片:拉详情
ion rpc --session sess_huge --method get_turn_detail --params '{"turnId": 100}'

# 5. 用户在详情里看到有意思的 tool_result,跳到消息流该位置
ion rpc --session sess_huge --method get_messages --params '{"before": "msg_850", "limit": 20}'
```

### F3 回滚后恢复查看

```bash
# 1. 用户执行了回滚(假设回滚到 msg_035)
# 2. 重新打开会话:get_tree 看分支现状
ion rpc --session sess_xxx --method get_tree
# → branches 里 msg_035 标记为 active

# 3. 默认 live 视点拉消息,自动是回滚后的
ion rpc --session sess_xxx --method get_messages
# → 返回 root→msg_035 路径

# 4. 用户想看回滚前的:切 branch 视点
ion rpc --session sess_xxx --method get_messages --params '{"view": "branch:msg_042"}'
```

### F4 导出全量历史(审计/备份)

```bash
# 一次性全量导出,含所有分支所有压缩前数据
ion rpc --session sess_xxx --method get_messages --params '{"view": "full", "limit": 0}'

# 或者分页导出(超大文件)
ion rpc --session sess_xxx --method get_messages --params '{"view": "full", "limit": 200}'
# 循环用 nextCursor 直到 hasMore=false
```

---

## Group G:旁路数据(custom / system_event)

> **场景**:会话中存在不进 LLM 上下文、但需要还原到前端或隐藏审计的旁路 entry。例如:扩展记忆搜索过程、压缩中的内部状态、重试次数过多的事件。这些通过 `include_custom` 参数控制是否随消息一起拉取。

### 背景说明

会话 JSONL 里除 `message` 类型外,还有五类旁路 entry,分两组:

**第一组:纯旁路数据(custom / system_event)** —— 不改变消息可见性

| entry type | 字段 | 用途 | 例子 |
|------------|------|------|------|
| `custom` | `customType`, `data`, 无 `display` | 纯 UI/旁路数据 | 扩展保存的学习记录、记忆搜索过程 |
| `custom_message` | `customType`, `content`, `display:bool`, `details` | 扩展消息(设计上可进 LLM,生产路径预留) | display=true 的可展示给用户 |
| `system_event` | `customType`, `label`, `display:bool` | 系统/模型切换事件 | display=true 的模型切换,display=false 的重试过多/压缩中 |

**第二组:改变消息可见性(deletion / segment_summary)** —— 这两类 entry 本身不是消息,但会**改变其他消息的可见性**

| entry type | 字段 | 用途 | 对拉取的影响 |
|------------|------|------|------------|
| `deletion` | `targetIds: string[]` | 软删除一批消息 | 被 targetIds 命中的消息**从结果中移除** |
| `segment_summary` | `targetIds: string[]`, `summary` | 把一批消息折叠成一条摘要 | 被 targetIds 命中的消息**替换为一条 BranchSummary 消息** |

**关键区别(display)**:
- `display: true` → 前端**应该展示**(用户可见的系统行为)
- `display: false` → 前端**隐藏**(内部状态、重试、自动压缩),只在细致排查时才拉

> 第二组(deletion/segment_summary)的影响是**默认生效**的——即使 `include_custom:"none"`,被删除的消息也不会出现在结果里。这是为了保证拉取结果和 LLM 实际看到的上下文一致。

### G1 默认拉取不含旁路数据

**场景**:正常聊天,不关心扩展/系统事件,只要消息正文。

```bash
ion rpc --session sess_custom --method get_messages
# include_custom 默认 "none"
```

**验证点**:
- ✅ `messages` 只含 `type:"message"` 的 entry
- ✅ 不含任何 custom/system_event 数据
- ✅ 性能最优(不过滤旁路数据)

### G2 拉取可展示的旁路数据(display:true)

**场景**:前端要还原"模型切换了""压缩完成了"等用户可见的系统行为,但不要内部的噪音。

```bash
ion rpc --session sess_custom --method get_messages \
  --params '{"include_custom": "display_only"}'
```

**预期响应(旁路数据混在 messages 里,按 entryId 顺序)**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_001", "type": "message", "role": "user", "content": "..." },
      { "entryId": "msg_002", "type": "message", "role": "assistant", "content": "..." },
      { "entryId": "sev_003", "type": "system_event", "customType": "model_change",
        "label": "切换到 glm-4.6", "display": true },
      { "entryId": "msg_004", "type": "message", "role": "user", "content": "..." }
    ],
    "hasMore": false,
    "totalCount": 4,
    "view": "live"
  }
}
```

**验证点**:
- ✅ 带 `display:true` 的 system_event / custom_message 混入 messages
- ✅ `display:false` 的隐藏事件**不返回**(如重试过多、压缩中)
- ✅ 按 entryId 时间顺序混排,前端按 type 渲染不同卡片

### G3 拉取全部旁路数据(细致排查)

**场景**:排查"为什么压缩了""重试了几次""扩展执行了什么",需要看隐藏的内部事件。

```bash
ion rpc --session sess_custom --method get_messages \
  --params '{"include_custom": "all"}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_001", "type": "message", "role": "user", "content": "..." },
      { "entryId": "cst_002", "type": "custom", "customType": "memory_search",
        "data": { "query": "重构", "matched": 3 }, "display": false },
      { "entryId": "sev_003", "type": "system_event", "customType": "retry_exceeded",
        "label": "LLM 重试 5 次", "display": false },
      { "entryId": "sev_004", "type": "system_event", "customType": "compacting",
        "label": "正在压缩...", "display": false },
      { "entryId": "msg_005", "type": "message", "role": "assistant", "content": "..." }
    ],
    "hasMore": false,
    "totalCount": 5,
    "view": "live"
  }
}
```

**验证点**:
- ✅ 所有旁路 entry 都返回(含 display:false 的隐藏事件)
- ✅ 前端可用 display 字段决定折叠/隐藏
- ✅ 适合 debug/审计场景

### G4 旁路数据与 turn 概览的关系

**场景**:`list_turns` 默认不统计旁路数据。如果某些 custom entry 属于某轮 turn 内部(如该轮触发的记忆搜索),用 `get_turn_detail` 展开。

```bash
# list_turns 的 entryCount 只算 message,不含 custom
ion rpc --session sess_custom --method list_turns --params '{"limit": 0}'
# → turns[0].entryCount = 2(只算 user+assistant 消息)

# get_turn_detail 展开,默认也只含 message
ion rpc --session sess_custom --method get_turn_detail \
  --params '{"turnId": 1}'

# 想看该轮的旁路数据,加 include_custom
ion rpc --session sess_custom --method get_turn_detail \
  --params '{"turnId": 1, "include_custom": "all"}'
# → entries 里混入该轮的 custom/system_event
```

**验证点**:
- ✅ `list_turns` 的统计不含旁路数据(保持轻量)
- ✅ `get_turn_detail` 支持 `include_custom`,展开该轮全部细节

### G5 customType 分类与两维属性(待落地)

当前 `display` 是预留的死字段。真正落地时,每个 customType 要标注**两个正交属性**,不是单一 `display` 标志:

| 属性 | 含义 | 取值 |
|------|------|------|
| **进 LLM 上下文?** | 是否发给模型 | 进 / 不进 |
| **展示给用户?** | 前端是否渲染 | 展示 / 不展示 |

这两个维度独立组合出 4 类:

| 组合 | 语义 | 例子 |
|------|------|------|
| 进 + 展示 | 正常对话/可见注入 | user 消息、用户主动 memory_save |
| 进 + 不展示 | 背后默默注入(模型看到,用户不看到) | 记忆自动注入 `<memory_context>`、系统提示 |
| 不进 + 展示 | 纯 UI 事件(模型不看到,用户看到) | 模型切换、压缩完成提示 |
| 不进 + 不展示 | 纯内部状态(都不看,细致排查才拉) | LLM 重试、压缩中、后台进程启动 |

**customType 分两类——内核不独占命名空间:**

**内核 customType**(内核定义,占用内核命名空间):

| customType | 进 LLM? | 展示? | 触发时机 |
|-----------|---------|-------|---------|
| `model_change` | 不进 | 展示 | 用户/系统切换模型 |
| `compaction_done` | 不进 | 展示 | compaction 成功 |
| `compacting` | 不进 | 不展示 | compaction 进行中(内部状态) |
| `llm_retry` | 不进 | 不展示 | stream_with_retry 重试(内部状态) |
| `retry_exceeded` | 不进 | 不展示 | 重试达上限(内部状态) |
| `process_started` | 不进 | 不展示 | bash 后台进程启动(内部状态) |

**插件 customType**(各扩展自定义,**不占用内核命名空间**,由扩展自己声明进 LLM?/展示?):

| 来源 | customType(举例) | 进 LLM? | 展示? | 说明 |
|------|-----------------|---------|-------|------|
| Memory 扩展 | `memory_saved` | 不进 | 展示 | 用户主动 memory_save(可见操作) |
| Memory 扩展 | `memory_search` | 不进 | 不展示 | on_input 自动检索(背后默默执行) |
| Memory 扩展 | `memory_injected` | 进 | 不展示 | on_context 注入 `<memory_context>`(模型看,用户不看到) |
| 任意扩展 | `<extension>_<event>` | 由扩展声明 | 由扩展声明 | 扩展在自己的 MANUAL.md 里定义 |

> **关键**:G5 表里的"进 LLM?/展示?"是**建议值**,不是死规定。插件 customType 的两维属性由**扩展自己声明**(在扩展的 MANUAL.md 里),内核不替插件决定。内核只管自己的 customType。拉取时,`include_custom` 过滤依据这两维:
> - `none`:全过滤
> - `display_only`:只带"展示"的
> - `all`:全带(含不展示的内部状态)

### G6 deletion:软删除后的拉取(默认过滤)

**场景**:用户在 TUI 里删了几条消息(比如删错了的 tool_result),之后拉取应该看不到被删的。deletion 是**默认生效**的过滤,即使 `include_custom:"none"`。

```bash
# 假设原会话有 msg_001~005,用户删了 msg_003(产生一条 deletion entry)
# 拉取默认就过滤掉被删的
ion rpc --session sess_deleted --method get_messages --params '{"limit": 0}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_001", "type": "message", "role": "user", "content": "..." },
      { "entryId": "msg_002", "type": "message", "role": "assistant", "content": "..." },
      { "entryId": "msg_004", "type": "message", "role": "assistant", "content": "..." },
      { "entryId": "msg_005", "type": "message", "role": "user", "content": "..." }
    ],
    "totalCount": 4,
    "view": "live"
  }
}
```

**验证点**:
- ✅ msg_003 不在结果里(被 deletion 过滤)
- ✅ `include_custom:"none"`(默认)也照样过滤(可见性影响是强制的,不是旁路可选项)
- ✅ 想看被删的,用 `view:"full"`(全量视点不过滤 deletion)或单独查 deletion entry

### G7 segment_summary:折叠后的拉取(替换为摘要)

**场景**:用户或系统把一批消息(比如 msg_010~020)折叠成一条摘要,之后拉取时这批消息被替换成一条 BranchSummary。

```bash
# 假设 msg_010~020 被 segment_summary 折叠,摘要为"前 5 轮的文件探索"
ion rpc --session sess_segmented --method get_messages --params '{"limit": 0}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_009", "type": "message", "role": "user", "content": "..." },
      {
        "entryId": "ss_030",
        "type": "branch_summary",
        "summary": "前 5 轮的文件探索(折叠了 msg_010~020)",
        "targetIds": ["msg_010", "msg_011", "...", "msg_020"]
      },
      { "entryId": "msg_021", "type": "message", "role": "user", "content": "..." }
    ],
    "totalCount": 3,
    "view": "live"
  }
}
```

**验证点**:
- ✅ msg_010~020 不在结果里,被一条 BranchSummary 替换
- ✅ 摘要带 `targetIds`(前端可点开"展开原消息")
- ✅ 这与 compaction 不同:compaction 是压缩**整个早期上下文**,segment_summary 是折叠**用户选定的一段**
- ✅ 想看折叠前的原消息,用 `view:"full"`

---

## Group H:turn 完整性专项

> **场景**:分页边界切在一轮 turn 中间时,如何保证返回的每轮 turn 都是完整的。

### H1 默认保证 turn 完整(complete_turn:true)

**场景**:limit=20 时,第 18-22 条属于 turn 3(1 个 user + 4 个 assistant/tool),分页边界切在中间。

```bash
# 默认 complete_turn:true
ion rpc --session sess_turn --method get_messages \
  --params '{"limit": 20}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* msg_001 ~ msg_022(22 条而非 20 条) */ ],
    "hasMore": true,
    "totalCount": 100,
    "nextCursor": "msg_022",
    "view": "live",
    "pageInfo": {
      "requestedLimit": 20,
      "actualCount": 22,
      "completedTurnBoundary": "msg_022"
    }
  }
}
```

**验证点**:
- ✅ 实际返回 22 条(多带 2 条凑齐 turn 3)
- ✅ 最后一条(msg_022)是 turn 3 的最后一条 entry
- ✅ `nextCursor` 指向 turn 3 结尾,下一页从 turn 4 开始
- ✅ `pageInfo` 说明实际条数和 turn 边界

### H2 关闭 turn 完整性(严格按 limit 切)

**场景**:纯流式渲染,不关心 turn 边界,要精确控制条数。

```bash
ion rpc --session sess_turn --method get_messages \
  --params '{"limit": 20, "complete_turn": false}'
```

**验证点**:
- ✅ 严格返回 20 条
- ✅ 最后一条可能是 turn 中间的 entry(半截 turn)
- ✅ 适合无限滚动纯文本流,不适合按 turn 折叠的 UI

### H3 turn 完整性与反向分页

**场景**:向上滚加载,`before` 游标也需要保证 turn 完整。

```bash
# 向上加载,complete_turn 保证起始边界也是 turn 整数倍
ion rpc --session sess_turn --method get_messages \
  --params '{"before": "msg_050", "limit": 20}'
# 如果 msg_031~035 属于 turn 8,自动从 msg_031 开始(而非 msg_030)
```

**验证点**:
- ✅ 反向分页也保证 turn 完整(起始和结束都是 turn 边界)
- ✅ 不会出现"上一页结尾半截 turn + 这一页开头半截 turn"的拼接问题

---

## Group I:历史拉取 + 实时事件拼接

> **场景**:前端真实工作流是 **先拉历史,再订阅实时事件**。这两路数据不能直接拼接——pi 用整章讲这个坑(rpc-data-guide.md),ION 必须说清楚。本组定义历史和实时如何衔接、entryId 如何在两路间保持一致。

### 背景:两路数据的职责划分

| 数据类型 | 来源 | 获取方式 |
|---------|------|---------|
| **历史消息**(完整 entry) | JSONL 文件 | `get_messages` 等 pull 接口 |
| **实时增量**(流式 token / 工具调用) | 运行中 agent | `subscribe` 事件流(push) |

**关键:两路用同一套 entryId**。历史里 `msg_042` 和实时事件流里最终提交的 `msg_042` 是同一条,可以安全去重。

### I1 历史拉取后再接实时(标准拼接)

**场景**:前端打开会话 → `get_messages` 拉历史 → `subscribe` 接实时 → 新消息不重复、不丢。

```bash
# 1. 先拉历史,拿到最后一条 entryId(比如 msg_042)
ion rpc --session sess_xxx --method get_messages --params '{"limit": 50}'
# → nextCursor: null, totalCount: 42, 最后一条 entryId: "msg_042"

# 2. 订阅实时事件流
ion subscribe --session sess_xxx
# → 收到 agent_start / text_delta / tool_call / agent_end 事件

# 3. agent 跑完一轮,产生新消息 msg_043~045
# 事件流里会带这些 entryId
```

**预期事件流(订阅端)**:

```
{"type":"event","event":{"type":"agent_start","turnId":11}}
{"type":"event","event":{"type":"text_delta","delta":"好的","entryId":"msg_043"}}
{"type":"event","event":{"type":"text_delta","delta":",我来","entryId":"msg_043"}}
{"type":"event","event":{"type":"tool_call","entryId":"msg_044","tool":"read"}}
{"type":"event","event":{"type":"agent_end","turnId":11}}
```

**拼接规则**:
- ✅ 历史最后一条 `msg_042`,实时事件从 `msg_043` 开始,**直接 append,无重复**
- ✅ 多个 `text_delta` 共享同一个 `entryId`(msg_043),前端按 entryId 聚合成一条消息
- ✅ `agent_end` 后,可重新 `get_messages` 拉到确认态的 msg_043~045(与事件流一致)

### I2 实时事件里缺失的信息(只能靠拉取补)

**场景**:有些 entry 类型**只出现在历史拉取里,实时事件流不推**。前端不能只靠订阅。

```bash
# 实时事件流不推这些:
# - model_change(模型切换)
# - thinking_level_change(思考级别)
# - compaction(压缩点)
# - deletion / segment_summary
#
# 前端要显示这些,必须定期/事件触发后重新 get_messages
```

| 信息 | 历史拉取 | 实时事件 | 说明 |
|------|---------|---------|------|
| user/assistant 消息 | ✅ | ✅ | 两路都有 |
| text_delta(流式 token) | ❌ | ✅ | 只在实时 |
| model_change | ✅ | ❌ | 只在历史 |
| compaction 点 | ✅ | ❌ | 只在历史 |
| deletion / segment_summary | ✅ | ❌ | 只在历史 |

**验证点**:
- ✅ 前端不能假设"订阅就够了",必须定期拉历史补 model_change/compaction/deletion
- ✅ 推荐策略:`agent_end` 事件后触发一次 `get_messages`(增量或全量)

### I3 断线重连后的历史补齐

**场景**:前端订阅中途断线,漏了一段事件,重连后用 `get_messages` 补齐。

```bash
# 断线前最后看到的 entryId 是 msg_050
# 重连后,从 msg_050 之后拉历史补齐
ion rpc --session sess_xxx --method get_messages \
  --params '{"after": "msg_050", "limit": 50}'
# → 返回 msg_051 起的消息,把断线期间漏的补上
```

**验证点**:
- ✅ `after` 游标用于"从某条之后"的正向补齐
- ✅ 补齐的消息与实时事件流里同一 entryId 的内容一致
- ✅ 配合 I1 的去重,断线不丢数据

---

## Group J:中断态 / 错误态拉取

> **场景**:会话被 abort、LLM 报错、工具失败、压缩失败。被中断的 turn 拉出来长什么样?错误信息在哪?前端怎么区分"正常结束"和"被打断"?——真实会话的高频调试区。

### J1 用户 abort 后的半截消息

**场景**:用户在 agent 输出到一半时按了 abort,最后一条 assistant 消息是半截的。

```bash
# 触发 abort
ion rpc --session sess_xxx --method abort

# 拉取,看被中断的 turn 长什么样
ion rpc --session sess_xxx --method get_messages --params '{"limit": 10}'
```

**预期响应(最后一条是半截 assistant)**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_050", "type": "message", "role": "user", "content": "继续" },
      {
        "entryId": "msg_051",
        "type": "message",
        "role": "assistant",
        "content": "好的,我来重构这",
        "stopReason": "aborted",
        "isComplete": false
      }
    ],
    "view": "live"
  }
}
```

**验证点**:
- ✅ 半截消息仍保存(已输出的部分不丢)
- ✅ `stopReason: "aborted"` 标记中断原因(前端可显示"已中断")
- ✅ `isComplete: false` 区分正常完成和被打断

### J2 stopReason 枚举(完整状态机)

turn / 消息的 `stopReason` 取值:

| stopReason | 含义 | 触发 |
|------------|------|------|
| `"completed"` | 正常完成 | assistant 输出完毕,无 tool_call |
| `"tool_use"` | 正常,调用工具后继续 | assistant 返回 tool_call |
| `"aborted"` | 用户中断 | abort RPC |
| `"error"` | 出错 | LLM 报错 / 工具失败 |
| `"max_turns"` | 达到轮数上限 | max_turns 到达 |
| `"compaction_failed"` | 压缩失败 | emergency truncate fallback |

**验证点**:
- ✅ 每条 assistant 消息都带 stopReason
- ✅ 前端按 stopReason 渲染不同状态(完成✓ / 中断⚠ / 错误✗)

### J3 LLM 报错的 turn

**场景**:LLM API 返回错误(限流/超时/格式错),重试 5 次后放弃,错误信息记录在哪。

```bash
# 假设上一轮 LLM 重试 5 次失败
ion rpc --session sess_xxx --method get_messages \
  --params '{"limit": 5, "include_custom": "all"}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [
      { "entryId": "msg_050", "type": "message", "role": "user", "content": "继续" },
      {
        "entryId": "sev_051",
        "type": "system_event",
        "customType": "retry_exceeded",
        "label": "LLM 重试 5 次失败: Rate limited",
        "display": false,
        "error": { "code": "rate_limited", "attempts": 5, "lastError": "429 Too Many Requests" }
      },
      {
        "entryId": "msg_052",
        "type": "message",
        "role": "assistant",
        "content": "",
        "stopReason": "error",
        "isComplete": false,
        "error": "LLM 重试 5 次失败"
      }
    ],
    "view": "live"
  }
}
```

**验证点**:
- ✅ 错误详情在 `system_event`(display:false,需 `include_custom:"all"` 才看到)
- ✅ 对应的 assistant 消息 `stopReason:"error"` / `isComplete:false`
- ✅ 错误的 turn 在 `list_turns` 里 `status:"error"`

### J4 工具调用失败的 turn

**场景**:agent 调用工具(如 bash)失败了,这个失败的 tool_call 和错误结果都在。

```bash
ion rpc --session sess_xxx --method get_turn_detail --params '{"turnId": 5}'
```

**预期响应(含失败的 tool_result)**:

```json
{
  "type": "response",
  "command": "get_turn_detail",
  "success": true,
  "data": {
    "turnId": 5,
    "entries": [
      { "entryId": "msg_020", "type": "message", "role": "assistant",
        "toolCall": { "tool": "bash", "command": "rm /system" } },
      {
        "entryId": "msg_021",
        "type": "message",
        "role": "toolResult",
        "content": "Permission denied: cannot remove '/system'",
        "isError": true
      },
      { "entryId": "msg_022", "type": "message", "role": "assistant",
        "content": "抱歉,删除失败了,权限不足。",
        "stopReason": "completed", "isComplete": true }
    ],
    "summary": { "status": "completed", "toolCallCount": 1 }
  }
}
```

**验证点**:
- ✅ 失败的 `tool_result` 带 `isError: true`(前端可标红)
- ✅ 错误信息在 content 里(完整保留)
- ✅ turn 整体 status 仍是 completed(工具失败不等于 turn 失败,agent 可能自己处理了)

### J5 压缩失败的 emergency fallback

**场景**:压缩时 LLM summarizer 失败,触发 emergency truncate(硬截断)。这个降级要在拉取时可见。

```bash
ion rpc --session sess_xxx --method get_messages \
  --params '{"include_custom": "all"}'
```

**预期响应(含降级标记的 compaction)**:

```json
{
  "type": "response",
  "command": "get_messages",
  "success": true,
  "data": {
    "messages": [ /* 压缩后的消息 */ ],
    "compactionPoints": [
      {
        "entryId": "cmp_005",
        "summary": "[emergency truncate] 上下文超过阈值,硬截断前 80 条",
        "tokensBefore": 32000,
        "stage": "emergency",
        "isFallback": true
      }
    ]
  }
}
```

**验证点**:
- ✅ compaction 的 `stage:"emergency"` / `isFallback:true` 标识降级
- ✅ summary 里写明是硬截断(前端可警告"历史被强制截断,可能丢失上下文")
- ✅ 对比正常压缩的 `stage:"single"` / `stage:"batched_merged"`

---

## Group K:统计聚合

> **场景**:用户想看"这个会话花了多少钱、用了多少 token、跑了多久""哪轮特别贵"。`list_turns` 的 turn 摘要里已带 tokens/cost/duration,会话级总额靠聚合。

### K1 会话级 token / 成本 / 时长汇总

**场景**:前端显示会话概览"总共 8 轮,花费 $0.42,跑了 12 分钟"。

```bash
# 方式 1:get_session_stats(轻量,直接从 index 读,不读消息)
ion rpc --session sess_xxx --method get_session_stats

# 方式 2:list_turns 全量拉,前端求和(适合按 turn 分布展示)
ion rpc --session sess_xxx --method list_turns --params '{"limit": 0}'
```

**方式 1 响应**:

```json
{
  "type": "response",
  "command": "get_session_stats",
  "success": true,
  "data": {
    "messageCount": 42,
    "turnCount": 8,
    "tokens": { "input": 12500, "output": 8300, "cacheRead": 2000 },
    "cost": 0.42,
    "durationMs": 720000,
    "errorCount": 1,
    "compressCount": 1
  }
}
```

**验证点**:
- ✅ 会话级总额从 index 读(O(1),不扫消息)
- ✅ 含 cost(成本)和 durationMs(总时长)
- ✅ `errorCount` / `compressCount` 反映会话健康度

### K2 单轮成本定位(哪轮最贵)

**场景**:用户觉得这次会话花费异常,想看哪轮 token 消耗最大。

```bash
ion rpc --session sess_xxx --method list_turns --params '{"limit": 0}'

# 返回每轮的 tokens:{input,output}
# 前端按 tokens 排序,高亮最贵的轮次
```

**验证点**:
- ✅ 每个 turn 摘要带独立 tokens(前端可排序/高亮)
- ✅ 定位到异常轮后,用 `get_turn_detail(turnId)` 展开看细节

---

## Group L:会话血缘(加载历史时知道数据从哪来)

> **场景**:fork 出来的会话,加载历史时能看到"这数据从哪来"。血缘是**底层存储时顺带记的元信息**,不是拉取的核心功能。fork 的完整实现(溯源 compaction、复制有效上下文)是独立设计,这里只关心拉取时怎么暴露血缘。
>
> **关系模型(极简)**:血缘**只一层、单向**。每个会话最多一个 `parentSession`(fork 来源),查 children 靠反向索引 O(1)。不传递祖先链,不做深层血缘树。

### 背景说明

| 概念 | 说明 |
|------|------|
| **根会话** | 从零开始,`parentSession: null`。大多数会话都是根会话 |
| **fork 出来的会话** | 借用了某会话的**当前有效上下文**(since_compaction 段),`parentSession` 指向源会话 |
| **parentEntry** | 从源会话哪条 entry 裂变。不填 = fork 当前全部有效上下文(= copy 语义) |
| **parentType** | 暂只有 `"fork"`。copy/fork-from-leaf 统一为 fork,靠 parentEntry 区分 |
| ~~copy~~ | 不单列。fork 不填 parentEntry = copy 当前上下文 |

### L1 加载会话时看血缘(向上查来源)

**场景**:用户打开一个会话,想知道它是不是从别处 fork 来的。

```bash
# list_sessions 已带血缘字段(F0),或单独查某个会话的元信息
ion rpc --method list_sessions
# 或
ion rpc --session sess_def --method get_session_stats
```

**预期响应(带血缘字段)**:

```json
{
  "type": "response",
  "command": "get_session_stats",
  "success": true,
  "data": {
    "messageCount": 12,
    "turnCount": 3,
    "parentSession": "sess_abc",
    "parentType": "fork",
    "parentEntry": "msg_035",
    "hasChildren": false,
    "childCount": 0
  }
}
```

**验证点**:
- ✅ `parentSession:"sess_abc"` 说明这个会话是 fork 来的
- ✅ `parentEntry:"msg_035"` 说明从 sess_abc 的 msg_035 处裂变
- ✅ 根会话这三个字段(`parentSession`/`parentType`/`parentEntry`)全为 `null`
- ✅ 血缘字段**从 index 读**(O(1)),不需要打开 JSONL 文件

### L2 查某会话的子会话(向下查继承)

**场景**:用户在一个会话里想看"我有没有 fork 出去别的发展方向"。

```bash
# 查直接子会话(O(1),走反向索引)
ion rpc --method get_children --params '{"session": "sess_abc"}'
```

**预期响应**:

```json
{
  "type": "response",
  "command": "get_children",
  "success": true,
  "data": {
    "children": [
      {
        "id": "sess_def",
        "name": "重构消息拉取接口(分支2)",
        "parentEntry": "msg_035",
        "turnCount": 3,
        "updatedAt": "2026-07-08T11:00:00Z"
      }
    ]
  }
}
```

**验证点**:
- ✅ 返回**直接子会话**(不递归孙会话——血缘只一层)
- ✅ O(1) 反向索引(`children_by_parent` Map),不扫文件
- ✅ 每条带 parentEntry(从父会话哪条裂变)+ 基本元信息
- ✅ 无子会话时返回空数组

### L3 从子会话跳到父会话的历史

**场景**:在 fork 出来的会话 sess_def 里,想看父会话 sess_abc 在 fork 点(msg_035)之前/之后的历史。

```bash
# 1. 从 sess_def 的血缘知道:parent=sess_abc, fork 点=msg_035
# 2. 切到父会话,拉 fork 点附近的消息
ion rpc --session sess_abc --method get_messages \
  --params '{"before": "msg_035", "limit": 20}'

# 3. 或看父会话 fork 点之后怎么发展的(sess_def 里看不到的)
ion rpc --session sess_abc --method get_messages \
  --params '{"after": "msg_035", "limit": 20}'
```

**验证点**:
- ✅ 子会话只含 fork 点(有效上下文)之前的数据,fork 点之后父会话的发展**不在子会话里**
- ✅ 想看父会话后续,必须切到父会话(用 parentSession 拿到 id)
- ✅ entryId 在两个会话间**不全局唯一**(fork 时保留原 ID),所以查询必须带 `(session_id, entry_id)` 二元组

### L4 fork 的有效上下文边界

**场景**:fork 出来的 sess_def,它的初始内容是什么?——是 sess_abc fork 点的**有效上下文**(since_compaction 段),不是全部历史。

```
父会话 sess_abc:
  msg_001~010          ← 已压缩(死数据)
  cmp_010 (summary)    ← 压缩点
  msg_011~035          ← 有效上下文(fork 点)
  msg_036+             ← fork 后新发展

fork 点 = msg_035
  ↓ 向前溯源到最近 compaction
  ↓ 遇到 cmp_010
  ↓
子会话 sess_def 初始内容:
  [CompactionSummary(cmp_010)] + [msg_011~035]
```

**验证点**:
- ✅ sess_def 的第一条是继承的 CompactionSummary(不是 msg_001 原始消息)
- ✅ sess_def 里**没有** msg_001~010(已被压缩,不再有效)
- ✅ sess_def 里**没有** msg_036+(fork 点之后,不属于它)
- ✅ 这就是"fork 当前有效上下文"的语义:fork = since_compaction 段的快照

> **fork 完整实现是独立设计**(溯源 compaction、复制有效上下文、标记 parentEntry),见 `docs/design/SESSION_TREE.md`。拉取文档只负责:加载历史时暴露血缘元信息(parentSession/parentEntry)+ 支持跨会话跳转查父会话。

---

## 用例与接口映射总表

| Group | Case | 接口 | view | 分页 | 粒度 | 旁路 |
|-------|------|------|------|------|------|------|
| A | 全量消息 | `get_messages` | live | limit:0(全量) | 全消息 | none |
| A | turn 概览 | `list_turns` | live | limit:0(全量) | turn 概览 | — |
| A | 用户输入 | `list_inputs` | live | limit:0(全量) | user 输入 | — |
| B | 最新一批 | `get_messages` | live | limit:50(默认) | 全消息 | none |
| B | 向上滚加载 | `get_messages` | live | before 游标 | 全消息 | none |
| B | 导航跳转 | `get_messages` | live | before 游标 | 全消息 | none |
| B | turn 概览分页 | `list_turns` | live | before 游标 | turn 概览 | — |
| C | 渲染卡片列表 | `list_turns` | live | limit | turn 概览 | — |
| C | 展开单轮 | `get_turn_detail` | — | 不分页 | 单轮明细 | 可选 |
| C | 详情跳消息流 | `get_messages` | live | before 游标 | 全消息 | none |
| D | 含压缩前 | `get_messages` | live | 分页/全量 | 全消息 | none |
| D | 只看压缩后 | `get_messages` | since_compaction | 分页/全量 | 全消息 | none |
| D | 多次压缩 | `get_messages` | since_compaction | 分页/全量 | 全消息 | none |
| E | 分支地图 | `get_tree` | — | 全量 | 树拓扑 | — |
| E | 指定分支 | `get_messages` | branch:\<id\> | 分页/全量 | 全消息 | 可选 |
| E | live 跟回滚 | `get_messages` | live | 分页/全量 | 全消息 | none |
| E | 全量审计 | `get_messages` | full | 分页 | 全消息 | 可选 all |
| F | 组合工作流 | 多接口 | 混合 | 混合 | 混合 | 混合 |
| **G** | **默认不含旁路** | `get_messages` | live | 分页 | 全消息 | **none** |
| **G** | **展示用旁路** | `get_messages` | live | 分页 | 全消息 | **display_only** |
| **G** | **全旁路排查** | `get_messages` | live | 分页 | 全消息 | **all** |
| **G** | **turn 详情含旁路** | `get_turn_detail` | — | 不分页 | 单轮明细 | **可选** |
| **G** | **deletion 过滤** | `get_messages` | live | 分页 | 全消息 | **默认过滤** |
| **G** | **segment_summary 折叠** | `get_messages` | live | 分页 | 含摘要 | **默认替换** |
| **H** | **turn 完整性(默认)** | `get_messages` | live | 分页+补齐 | 全消息 | none |
| **H** | **严格 limit** | `get_messages` | live | 分页不补齐 | 全消息 | none |
| **I** | **历史+实时拼接** | `get_messages`+`subscribe` | live | — | 全消息+事件 | — |
| **I** | **断线重连补齐** | `get_messages` | live | after 游标 | 全消息 | — |
| **J** | **abort 半截消息** | `get_messages` | live | 分页 | 含 stopReason | 可选 all |
| **J** | **LLM 报错 turn** | `get_messages`/`get_turn_detail` | live | — | 含 error | 可选 all |
| **J** | **工具失败** | `get_turn_detail` | — | 不分页 | 含 isError | — |
| **J** | **压缩降级** | `get_messages` | live | — | 含 isFallback | 可选 all |
| **K** | **会话级统计** | `get_session_stats` | — | — | 元数据 | — |
| **K** | **单轮成本定位** | `list_turns` | live | 全量 | turn 摘要+tokens | — |
| **L** | **看血缘(向上)** | `list_sessions`/`get_session_stats` | — | — | 元数据(含 parentSession) | — |
| **L** | **查子会话(向下)** | `get_children` | — | — | 直接子会话列表 | — |
| **L** | **跳父会话历史** | `get_messages` | live | before/after | 全消息(切到父会话) | — |

---

## 实现优先级建议

| 优先级 | Group | 价值 |
|--------|-------|------|
| **P0** | A(全量)+ B(分页) | 解决基本拉取 + 大会话,覆盖 90% 场景 |
| **P0** | **H(turn 完整性)** | 分页不切断 turn,保证 UI 正确渲染 |
| **P0** | **I(历史+实时拼接)** | 前端集成的头号需求,没有这步 UI 做不出来 |
| **P0** | **J(中断态/错误态)** | 真实会话高频区,abort/报错/降级必须可见 |
| **P1** | C(逐轮)+ D(压缩) | 前端卡片视图 + 压缩感知 |
| **P1** | **G(旁路+deletion/segment)** | 还原前端系统事件 + 可见性过滤 |
| **P1** | **K(统计聚合)** | 会话健康度 + 成本定位 |
| **P1** | **F0(会话入口衔接)** | list_sessions → 拉取的真实路径 |
| **P1** | **L(会话血缘)** | fork 来源标记 + 跨会话跳转(元信息,非核心拉取) |
| **P2** | E(分支) | 依赖 SESSION_TREE 落地 |
| **P3** | F(其余组合) | 上述就绪后自然支持 |
