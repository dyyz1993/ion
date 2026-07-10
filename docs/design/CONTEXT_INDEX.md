# Context Index — 上下文索引与快照折叠

> **状态：待定** — 设计稿已完成,待评审与排期。V1(文件级索引 + 确定性快照折叠)先行,V2(会话级主题漂移折叠)后续。

---

## 概览

ION 当前的上下文管理只有 Compaction(token 阈值触发的整体 LLM 摘要)。它有两个盲区:

1. **Agent 不知道上下文里塞了什么** —— 同一个文件被 `read` 多次(代码已改),多份不同版本的快照同时留在上下文里,不去重、不失效,越堆越臃肿。
2. **压缩粒度太粗** —— Compaction 只在 token 超阈值时整体 LLM 摘要,无法精准干掉"过期的那一条文件快照",要调 LLM、要花钱、还不准。

Context Index Extension 解决这两个盲区,分两个层次:

| 层次 | 做什么 | 触发方式 | 调 LLM 吗 |
|------|--------|---------|----------|
| **V1: 快照折叠** | 追踪 `read` 进上下文的文件,在 `write`/`edit` 覆盖后把旧快照替换成占位符 | 确定性(write 覆盖 read 即触发) | ❌ 零成本 |
| **V2: 会话折叠** | 检测主题漂移,把不再相关的整段对话历史折叠成摘要 | 语义性(距离衰减 / embedding) | 可选 |

本文档聚焦 **V1**。V2 在 §7 后续工作中记录设计方向。

### 能力清单

| 能力 | 入口 | V1 状态 |
|------|------|---------|
| 文件索引采集(read/write/edit) | `after_tool_call` 钩子 | 🔧 设计稿 |
| 文件新鲜度追踪(Stale 检测) | 扩展内部状态 | 🔧 设计稿 |
| 上下文 tree 查询 | `context_index_tree` LLM 工具 + `extension_rpc: tree` | 🔧 设计稿 |
| 上下文列表查询 | `context_list_files` LLM 工具 + `extension_rpc: list` | 🔧 设计稿 |
| 快照折叠(过期 read 替换为占位符) | `on_context` 钩子 | 🔧 设计稿 |
| 索引注入 system prompt | `on_system_prompt` 钩子 | 🔧 设计稿 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | `ContextIndex` 数据结构 | 🔧 | `cargo test --lib context_index_*` |
| 1.2 | `after_tool_call` 采集(read/write/edit) | 🔧 | `cargo test --lib context_index_record_read` |
| 1.3 | Stale 检测(write 标记旧 read 过期) | 🔧 | `cargo test --lib context_index_stale_detection` |
| 2.1 | `context_index_tree` LLM 工具 | 🔧 | e2e: LLM 调用返回 tree |
| 2.2 | `context_list_files` LLM 工具 | 🔧 | e2e: LLM 调用返回列表 |
| 2.3 | `extension_rpc: tree/list/ranges` | 🔧 | `ion rpc ... extension_rpc context tree` |
| 3.1 | `on_context` 快照折叠(Stale → 占位符) | 🔧 | `cargo test --lib context_index_fold_stale` |
| 3.2 | `on_system_prompt` 注入 `<context_index>` | 🔧 | e2e: system prompt 含索引 XML |
| 3.3 | 折叠不落盘(运行时优化,jsonl 完整保留) | 🔧 | `cargo test --lib context_index_no_persist` |

---

## 1. 定位:为什么是扩展,不是内核

按 AGENTS.md §内核 vs 扩展的设计方针:

> 策略/行为定制 → 扩展。内核要足够强大,让扩展只做策略层的事。

Context Index 的本质是**上下文优化策略**——"哪些文件快照过期了、该不该折叠",这是策略判断,不是基础设施。因此做成 **Extension**,通过 3 个钩子接入:

| 钩子 | 签名 | 职责 |
|------|------|------|
| `after_tool_call` | `(call: &ToolCall, result: &ToolResult)` | **采集**:记录 read/write/edit 到索引 |
| `on_context` | `(&mut Vec<Message>)` | **折叠**:把 Stale 的 read tool_result 替换成占位符 |
| `on_system_prompt` | `(&mut String)` | **注入**:把索引摘要写进 system prompt |

这三个钩子在 ION 中均已实现(见 [src/agent/extension.rs:99-124](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/extension.rs#L99-L124)),不需要新增内核钩子。

> **关键确认**:`on_context` 拿到的是 `&mut self.messages`(agent 自己的存储,不是副本),所以折叠改动**持久、跨 turn 生效**。调用点见 [src/agent/agent_loop.rs:469](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L469)。

---

## 2. 配置

```rust
pub struct ContextIndexConfig {
    /// 是否启用快照折叠(V1 核心功能)
    pub folding_enabled: bool,         // 默认 true

    /// 是否启用索引注入 system prompt
    pub inject_system_prompt: bool,    // 默认 true

    /// 折叠占位符的最大长度(超出截断)
    pub placeholder_max_len: usize,    // 默认 120

    /// 距离衰减阈值(V2 会话折叠用,V1 不启用)
    pub session_fold_turns: usize,     // 默认 0(不启用)
}
```

默认值通过 `config.json` 的 `extensions.context-index` 字段配置:

```json
{
  "extensions": {
    "context-index": {
      "enabled": true,
      "folding_enabled": true,
      "inject_system_prompt": true
    }
  }
}
```

---

## 3. 数据结构

### 3.1 核心索引

```rust
/// Context Index 扩展的内存状态(不落盘)
pub struct ContextIndex {
    /// 文件路径 → 该文件的所有读写记录
    pub files: HashMap<String, FileRecord>,

    /// 诚实声明的未索引来源
    pub untracked_sources: Vec<String>,  // ["grep", "bash"]
}

pub struct FileRecord {
    /// 该文件的所有 read 记录(可能多条,不同 turn 读过)
    pub reads: Vec<ReadRecord>,

    /// 该文件的所有 write/edit 记录
    pub writes: Vec<WriteRecord>,
}

pub struct ReadRecord {
    /// 哪一轮 turn 读的
    pub turn: u32,

    /// 读了哪些行(V1 = 1..全文行数;V2 支持 offset/limit)
    pub lines: LineRange,

    /// 读取时的内容 hash(djb2)
    pub content_hash: u64,

    /// 关联的 tool_result message_id(用于折叠时定位)
    pub message_id: String,

    /// 置信度(read 永远 high)
    pub confidence: Confidence,  // High

    /// 新鲜度状态
    pub status: Freshness,       // Current | Stale
}

pub struct WriteRecord {
    /// 哪一轮 turn 写的
    pub turn: u32,

    /// 写入后的内容 hash
    pub content_hash: u64,

    /// write 还是 edit
    pub kind: WriteKind,  // Write | Edit
}

pub enum Freshness {
    /// 当前有效:未被后续 write 覆盖
    Current,
    /// 已过期:被后续 write/edit 覆盖
    Stale { overwritten_by_turn: u32 },
}
```

### 3.2 为什么用 `message_id` 而不是消息下标

折叠时需要在 `Vec<Message>` 里找到对应的 `ToolResultMessage`。用 `message_id` 而不是下标,因为:

- Compaction 会重写整个 `Vec<Message>`(删除中间区),下标会失效。
- `message_id` 是稳定的标识符,Compaction 后仍然能匹配。

### 3.3 hash 算法

复用 Memory 扩展的 djb2 实现(`src/memory.rs:240`),简单高效,只用于判断"内容是否变了",不需要密码学安全性。

---

## 4. 主流程

### 4.1 采集(`after_tool_call`)

每次工具执行后,扩展根据 `call.name` 采集:

```
after_tool_call(call, result):
    match call.name:
        "read":
            path = call.arguments["file_path"]
            content = result.output
            hash = djb2(content)
            lines = count_lines(content)  // V1: 1..N
            record = ReadRecord { turn, lines, hash, message_id, status: Current }
            index.files[path].reads.push(record)

        "write":
            path = call.arguments["file_path"]
            // 标记该文件的所有旧 read 为 Stale
            for read in index.files[path].reads:
                if read.status == Current:
                    read.status = Stale { overwritten_by_turn: current_turn }
            index.files[path].writes.push(WriteRecord { turn, hash, kind: Write })

        "edit":
            path = call.arguments["file_path"]
            // 同 write,标记旧 read 为 Stale
            mark_reads_stale(index, path, current_turn)
            index.files[path].writes.push(WriteRecord { turn, hash, kind: Edit })

        "grep" | "bash" | "find":
            // 不索引,但 untracked_sources 已声明
            pass
```

### 4.2 快照折叠(`on_context`)

每轮发 LLM 前,遍历索引,把 `status == Stale` 的 read tool_result 替换:

```
on_context(messages):
    for msg in messages:
        if msg is ToolResult and msg.tool_name == "read":
            path = extract_path_from_call(msg.tool_call_id)
            record = index.files[path].reads.find(message_id == msg.id)
            if record && record.status == Stale:
                // 替换 content 为占位符
                msg.content = [
                    TextContent {
                        text: "[ContextIndex: {path} — 读于 turn {record.turn} ({lines}行),"
                              + "已被 turn {overwritten_by_turn} 的 {kind} 覆盖]\n"
                              + "[需要最新内容请重新 read {path}]"
                    }
                ]
```

### 4.3 占位符示例

折叠后,LLM 看到的内容变成:

```
[ContextIndex: src/agent/tool.rs — 读于 turn 3 (580行),已被 turn 9 的 write 覆盖]
[需要最新内容请重新 read src/agent/tool.rs]
```

LLM 看到这个,知道旧快照没了,可以重新 `read` 拿最新版本。

### 4.4 system prompt 注入(`on_system_prompt`)

```
on_system_prompt(prompt):
    tree = build_tree(index)
    prompt += "\n<context_index>\n{tree}\n</context_index>\n"
```

注入的 XML 长这样:

```xml
<context_index>
src/
├── agent/
│   ├── agent_loop.rs    [current · turn 12 · 全文 820 行]
│   ├── tool.rs          [STALE · turn 3,已被 turn 9 write 覆盖]
│   └── compact.rs       [current · turn 8 · 全文 697 行]
├── bin/
│   └── ion.rs           [current · turn 5 · 全文 2100 行]

注: grep/bash 读取的内容不在索引内
</context_index>
```

---

## 5. 查询接口

三种消费者,同一份数据:

### 5.1 LLM 工具

| 工具 | 参数 | 返回 | 用途 |
|------|------|------|------|
| `context_index_tree` | 无 | 目录树 + 新鲜度 | LLM 全局概览上下文里有哪些文件 |
| `context_list_files` | 无 | 文件列表 + 状态 | LLM 快速查"我读过哪些文件" |
| `context_file_ranges` | `path: String` | 该文件的行段 + 新鲜度 | LLM 精确查"这个文件哪些行有效" |

### 5.2 extension_rpc(CLI / 外部 UI)

```bash
# 查看树形索引
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"context-index","method":"tree"}'

# 查看列表
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"context-index","method":"list"}'

# 查看某文件的行段
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"context-index","method":"ranges","params":{"path":"src/foo.rs"}}'
```

**响应 JSON(tree)**:

```json
{
  "type": "response",
  "success": true,
  "data": {
    "files": [
      {"path": "src/agent/agent_loop.rs", "status": "current", "turn": 12, "lines": "1-820"},
      {"path": "src/agent/tool.rs", "status": "stale", "turn": 3, "overwritten_by_turn": 9}
    ],
    "untracked": ["grep", "bash"]
  }
}
```

---

## 6. 关键设计决策

### 6.1 为什么只追 read,不追 grep/bash

详见 [§8 对标分析](#8-对标分析pi-现状)。三种工具的可追踪性:

| 工具 | 可追踪性 | 原因 |
|------|---------|------|
| `read` | ✅ 高(参数即路径) | `arguments.file_path` 精确 |
| `grep` | ⚠️ 中(需解析输出) | 参数是搜索范围,精确文件要解析 `路径:行号:` 前缀 |
| `bash` | ❌ 低(不可靠) | `command` 是任意字符串,`cat $(find ...)` / 管道 / 变量无法解析 |

**决策**:V1 只追 `read`(100% 精确),grep/bash 在 `untracked_sources` 里诚实声明。

> 数据结构从开始就按"行段"设计(`ReadRecord.lines`),V2 升级到追 grep 时不用重构。

### 6.2 为什么折叠不落盘

| 方案 | 落盘 jsonl | 回滚 | 代价 |
|------|-----------|------|------|
| 折叠写进 jsonl | 改了历史行 | ❌ 历史被破坏 | 违反 only-append 不变量 |
| **折叠只在运行时(采用)** | **jsonl 永远完整** | ✅ 回滚/重启自动恢复折叠前状态 | 重启需重建索引(从 jsonl 回放 `after_tool_call`) |

折叠是**运行时上下文优化**,不是持久化变更。jsonl 里永远是完整原始历史,session_tree 的 branch/rollback 不受影响。

### 6.3 折叠 vs Compaction:互补不替代

```
快照折叠(确定性,零成本)  →  干掉过期文件快照,省大量 token
        │
        │  长会话剩余历史仍会涨
        ▼
Compaction(token 阈值,LLM 摘要)  →  兜底,超阈值时整体摘要
```

折叠让 Compaction 更少触发(因为 token 涨得慢了),也让 Compaction 摘要时不用处理一堆无用的旧文件内容。

### 6.4 Stale 判定规则

| 场景 | 判定 | 处理 |
|------|------|------|
| read 后无 write | `Current` | 保留 |
| read 后 write 全文 | `Stale` | 折叠 |
| read 后 edit 局部 | `Stale`(文件级) | 折叠(V1 不做行级 diff) |
| read 后又 read | 两条都 `Current`(最新一条关联最新 hash) | 都保留 |
| write 后 read | 新 read 为 `Current`,旧 read 仍可能 `Stale` | 各自独立判定 |

> V1 是**文件级**折叠:文件只要被 write/edit 动过,旧的全文 read 就整体标记 Stale。行级 diff("只折叠变更行")留 V2。

---

## 7. 后续工作

| # | 待办 | 优先级 | 说明 |
|---|------|--------|------|
| 1 | **V2: 会话级折叠(主题漂移检测)** | P1 | 距离衰减起步 → embedding 相似度(V2.1)→ LLM 判断(V2.2)。详见下方 |
| 2 | ~~**read 工具加 offset/limit**~~ | ✅ 已完成 | tool 层切片,加 `cat -n` 行号输出(`tool.rs` format_lines) |
| 3 | **追 grep(解析输出)** | P2 | 从 `untracked_sources` 移出,解析 ripgrep 的 `路径:行号:` 前缀 |
| 4 | **Compaction 对齐 pi** | P2 | ~~溢出恢复(A2)~~ ✅ 已完成 / 增量摘要(A5) / split-turn(A7) |
| 5 | **工具输出截断** | P3 | 工具输出超 50KB/2000 行自动截头尾(对齐 pi A6) |

### V2: 会话级折叠(设计方向)

V1 解决"过期文件快照"后,第二个膨胀源是**整段对话历史**。即使文件快照都新鲜,长会话里 30 轮前的讨论可能和当前任务无关。

| 检测方法 | 精度 | 成本 | 误折叠风险 |
|---------|------|------|-----------|
| ① 距离衰减 | 低 | 零 | 高(粗暴,可能折叠正在进行的任务) |
| ② 关键词/词频差异 | 中 | 低 | 中(误判) |
| ③ embedding 相似度 | 高 | 中(需 embedding 模型) | 低 |
| ④ LLM 判断 | 最高 | 高(每次判断调 LLM) | 最低 |

**推荐路线**:① 起步(距离衰减,让折叠跑起来)→ ③ embedding(V2.1,精度/成本比最优)→ ④ LLM 判断(V2.2,最终精度)。

折叠后的找回机制:从 jsonl 重新加载展开(因为 V1 的"折叠不落盘"原则同样适用)。

---

## 8. 对标分析:pi 现状

> 调研日期:2026-07-10。pi 源码:`/Users/xuyingzhou/Project/temporary/pi-momo-fork/`

### 8.1 pi 没有的(ION 原创增量)

| 能力 | pi 状态 | 证据 |
|------|---------|------|
| 上下文文件索引 | ❌ 不存在 | 全项目搜 `context_list_files` / `fileIndex` / `contextIndex` 零命中 |
| 快照折叠(read 过期检测) | ❌ 不存在 | write/edit 后不标记旧 read 结果 |
| 主题漂移检测 | ❌ 不存在 | 唯一的"检测"是 `tool-loop-detector`(重复工具调用),与主题无关 |

> pi 在 compaction 时会 `extractFileOpsFromMessage` 提取 read/write/edit 路径(`compaction/utils.ts:24-51`),但**只塞进摘要文本,没进一步用**。数据已经在那里,pi 没有往前走这一步。

### 8.2 pi 有、ION 缺的(Compaction 增量对齐)

| 能力 | pi | ION | 差距 |
|------|----|----|------|
| **溢出恢复** | ✅ `_checkCompaction` 检测 LLM 溢出错误码,自动 compaction 重试 5 次 | ❌ | ION 缺各厂商溢出正则匹配 |
| **增量摘要** | ✅ 已有 compaction 摘要时用 UPDATE 模式合并 | ❌ | ION 每次从零摘要 |
| **split-turn** | ✅ 切在 turn 中间时被截半条单独摘要 | ❌ | ION 只切消息边界 |
| **分支摘要** | ✅ 切换分支时离开的分支被 LLM 摘要 | ❌ | ION 有 session_tree 但无分支摘要 |
| **工具输出截断** | ✅ 超 50KB/2000 行截头尾 | ❌ | ION 无截断 |

### 8.3 ION 已对齐 pi 的

| 能力 | 状态 |
|------|------|
| 阈值压缩(token threshold + LLM 摘要) | ✅ 对齐 |
| Emergency truncate(超 2x context window) | ✅ 对齐 |
| Compaction safety(branch 穿越压缩点拒绝) | ✅ 对齐 |

---

## 9. CLI 测试指南(待实现)

> 实现后按 [CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md) 补充完整测试 case。

### Group A:索引采集

```bash
# A1 read 文件后索引记录该文件
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"context-index","method":"list"}'
```

**预期**:返回刚 read 的文件,status=current。

### Group B:快照折叠

```bash
# B1 read → write 同一文件后,旧 read 被折叠
# 预期:messages 里旧 read 的 content 变成占位符
# B2 read → edit 同一文件后,旧 read 被折叠
# 预期:同上
```

### Group C:查询接口

```bash
# C1 tree 查询返回目录树
# C2 list 查询返回文件列表
# C3 ranges 查询返回行段
```

### Group D:折叠不落盘

```bash
# D1 折叠后 jsonl 里旧 read 内容仍然完整
# D2 重启会话后索引重建,折叠重新生效
```

---

## 10. 关键 bug fix 记录

> 实现过程中踩过的坑,待补充。
