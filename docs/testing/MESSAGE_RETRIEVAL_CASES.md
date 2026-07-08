# 会话消息拉取 — CLI 用例集

> **状态:设计稿** — 基于 `get_messages` / `list_turns` / `list_inputs` / `get_turn_detail` / `get_tree` 五接口的 CLI 验证用例。
>
> 本文档只列**用户场景 + CLI 命令 + 预期结果**,不含 Rust 实现。实现细节见 `docs/design/MESSAGE_RETRIEVAL.md`(待补)。

---

## 接口速查

| 接口 | 粒度 | 分页 | 用途 |
|------|------|------|------|
| `get_messages` | 全消息 | ✅ entryId 游标 | 拉消息流(消息列表),含压缩前 |
| `list_turns` | turn 摘要 | ✅ turnId 游标 | 逐轮预览(前端卡片/时间线) |
| `list_inputs` | user 输入 | ✅ entryId 游标(默认全量) | 快速拉用户提问 |
| `get_turn_detail` | 单 turn 明细 | ❌ 不分页 | 展开某轮看全部细节 |
| `get_tree` | 树拓扑 | 默认全量 | 渲染会话分支地图 |

**公共参数(所有接口可选)**:

| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `view` | string | `"live"` | 视点:`live` / `since_compaction` / `branch:<leaf_id>` / `full` |
| `after` | string | — | 正向分页游标(entryId 或 turnId),向后翻页 |
| `before` | string | — | 反向分页游标,向上滚动加载历史 |
| `limit` | number | `50` | 每页条数。`0` = 全量(显式 opt-out 分页) |
| `complete_turn` | bool | `true` | 分页时保证每轮 turn 完整(边界不切断 turn) |
| `include_custom` | string | `"none"` | 旁路数据:见下方"分页规则" |

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

**3. 旁路数据(`include_custom`)**

会话中存在**不进 LLM 上下文但需要还原到前端**的旁路 entry:

| entry type | 用途 | 例子 |
|------------|------|------|
| `custom` | 纯 UI/旁路数据 | 扩展保存的学习记录、记忆搜索过程 |
| `custom_message` | 扩展消息(设计上进 LLM 但生产路径未用) | — |
| `system_event` | 系统/模型切换事件 | 压缩中、重试次数过多、模型切换 |

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

**场景**:前端想渲染"每轮对话一张卡片",不想要消息正文,只要概览。

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
        "userBrief": "帮我列出项目结构",
        "summary": "列出了 src/ tests/ docs/ 目录",
        "keySteps": ["ls", "read"],
        "toolCallCount": 2,
        "tokens": { "input": 150, "output": 200 },
        "durationMs": 1200,
        "entryCount": 2,
        "status": "completed"
      },
      {
        "turnId": 2,
        "userId": "msg_003",
        "userBrief": "再加一个测试目录",
        "summary": "创建了 tests/ 目录",
        "keySteps": ["bash"],
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

**验证点**:
- ✅ 每个 turn 一条摘要(无正文,轻量)
- ✅ `userBrief` 是用户提问(方便前端卡片标题)
- ✅ `summary` 一句话经过
- ✅ `keySteps` 工具调用序列(前端可渲染成图标)
- ✅ 比 `get_messages` 返回数据小得多(只含摘要)

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

前端拿到 `turns` 数组,每条渲染成一张卡片:标题=`userBrief`,副标题=`summary`,图标=`keySteps`,时间=`durationMs`。

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
    "summary": {
      "userBrief": "帮我重构消息拉取接口",
      "summary": "重构了 session 模块的消息拉取接口",
      "keySteps": ["read", "edit", "bash"],
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
- ✅ 同时返回 `summary`(与 list_turns 里的一致),前端可直接复用
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

### F1 新会话首次打开的标准流程

```bash
# 1. 拉会话树(渲染分支地图,极轻量)
ion rpc --session sess_xxx --method get_tree

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

会话 JSONL 里除 `message` 类型外,还有三种旁路 entry:

| entry type | 字段 | 用途 | 例子 |
|------------|------|------|------|
| `custom` | `customType`, `data`, 无 `display` | 纯 UI/旁路数据 | 扩展保存的学习记录、记忆搜索过程 |
| `custom_message` | `customType`, `content`, `display:bool`, `details` | 扩展消息(设计上可进 LLM,生产路径预留) | display=true 的可展示给用户 |
| `system_event` | `customType`, `label`, `display:bool` | 系统/模型切换事件 | display=true 的模型切换,display=false 的重试过多/压缩中 |

**关键区别**:
- `display: true` → 前端**应该展示**(用户可见的系统行为)
- `display: false` → 前端**隐藏**(内部状态、重试、自动压缩),只在细致排查时才拉

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

### G5 display 字段生产落地的场景(待实现)

当前 `display` 字段是预留的死字段。以下场景待落地:

| 场景 | customType | display | 触发时机 |
|------|-----------|---------|---------|
| 模型切换 | `model_change` | true | 用户/系统切换模型 |
| 压缩完成(用户可见) | `compaction_done` | true | compaction 成功 |
| 压缩中(隐藏) | `compacting` | false | compaction 进行中 |
| LLM 重试 | `llm_retry` | false | stream_with_retry 重试 |
| 重试过多 | `retry_exceeded` | false | 重试达上限 |
| 记忆搜索 | `memory_search` | false | Memory 扩展 on_input 检索 |
| 记忆注入 | `memory_injected` | false | Memory 扩展 on_context 注入 |
| 记忆保存 | `memory_saved` | true | 用户主动 memory_save |
| 后台 bash | `process_started` | false | bash 后台进程 |

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

## 用例与接口映射总表

| Group | Case | 接口 | view | 分页 | 粒度 | 旁路 |
|-------|------|------|------|------|------|------|
| A | 全量消息 | `get_messages` | live | limit:0(全量) | 全消息 | none |
| A | turn 概览 | `list_turns` | live | limit:0(全量) | turn 摘要 | — |
| A | 用户输入 | `list_inputs` | live | limit:0(全量) | user 输入 | — |
| B | 最新一批 | `get_messages` | live | limit:50(默认) | 全消息 | none |
| B | 向上滚加载 | `get_messages` | live | before 游标 | 全消息 | none |
| B | 导航跳转 | `get_messages` | live | before 游标 | 全消息 | none |
| B | turn 概览分页 | `list_turns` | live | before 游标 | turn 摘要 | — |
| C | 渲染卡片列表 | `list_turns` | live | limit | turn 摘要 | — |
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
| **H** | **turn 完整性(默认)** | `get_messages` | live | 分页+补齐 | 全消息 | none |
| **H** | **严格 limit** | `get_messages` | live | 分页不补齐 | 全消息 | none |

---

## 实现优先级建议

| 优先级 | Group | 价值 |
|--------|-------|------|
| **P0** | A(全量)+ B(分页) | 解决基本拉取 + 大会话,覆盖 90% 场景 |
| **P0** | **H(turn 完整性)** | 分页不切断 turn,保证 UI 正确渲染 |
| **P1** | C(逐轮)+ D(压缩) | 前端卡片视图 + 压缩感知 |
| **P1** | **G(旁路数据)** | 还原前端系统事件 + 细致排查 |
| **P2** | E(分支) | 依赖 SESSION_TREE 落地 |
| **P3** | F(组合) | 上述就绪后自然支持 |
