# Session Tree 验收规格

> **版本**：v1.0
> **本文档给独立评审方 / QA 阅读。** 所有用例通过统一的 **SessionTreeHarness** 执行，无需阅读源码、无需真实 LLM。
> **配套技术设计**：[docs/design/SESSION_TREE.md](../design/SESSION_TREE.md)

---

## 0. 这个功能是干什么的（一句话版）

让用户/Agent 在**同一个会话**里回退到任意一条历史消息，从那里分叉出一条新路径继续对话——原来的路径完整保留，随时可以切回去。

类似 `git branch`：会话历史变成一棵树，不是一条直线。

### 0.1 为什么要这个功能

长会话里常见场景：
- 探索方案 A 走了几步发现不对 → 想回到分叉点试方案 B，**但不想丢掉 A 的探索记录**
- Agent 自动跑了一轮发现方向错了 → 想回退重来，**但保留失败经验作参考**
- 同一个任务想并行试两种思路 → 分叉两条分支对比

### 0.2 核心原理（无技术细节）

会话文件是一个**只增不改**的日志（类似会计账本）。

- 每条消息都指向它的"上一条"（像链表）
- 多条消息可以指向同一条"上一条" → 形成树
- 有一个"光标"（leaf 指针）标记**当前活跃在哪条消息**
- **分叉 / 回滚 / 切换**都是同一件事：移动这个光标
- 光标的每次移动都会在日志里追加一条记录，**从不删除或修改任何旧记录**

所以这个功能天然满足：
- ✅ **历史永不丢失**——回滚掉的对话仍在文件里
- ✅ **操作可逆**——回滚本身也是追加，可以再"回滚这次回滚"
- ✅ **崩溃安全**——写到一半崩溃只丢最后一条，历史不损坏

---

## 1. Harness：所有用例的统一入口

> **重要：本文档所有用例都通过 `SessionTreeHarness` 执行，不直接调 `ion` CLI、不调真实 LLM、不污染 `~/.ion`。**

### 1.1 Harness 是什么

`SessionTreeHarness` 是一个测试脚手架，基于 **FauxProvider**（架构级 LLM Mock，对标 pi 的 `@dyyz1993/pi-ai` FauxProvider，详见 [docs/design/FAUX_PROVIDER.md](../design/FAUX_PROVIDER.md)）构建。它走**完整 agent 链路**（agent loop → session.jsonl 写入），但响应来自预设队列而非真实 LLM。

**设计哲学（对标 pi）：harness 是"原始对象的引用集合"，不是"方法封装层"。**

```
┌─────────────────────────────────────────────────────────┐
│  SessionTreeHarness（引用集合，对标 pi test harness）    │
│                                                         │
│  暴露的原始对象（测试直接操作，harness 不重新封装）：     │
│    .session_file  → SessionFile（直接调 get_tree/branch）│
│    .faux          → FauxProvider 控制柄（排响应/查次数） │
│    .events        → 事件收集数组                         │
│    .file_path     → JSONL 文件路径                       │
│                                                         │
│  少量便捷方法：                                          │
│    create()       → 临时目录 + 注册 faux + 空会话        │
│    load_from(p)   → 从文件加载（模拟重启）               │
│    seed(text,rs)  → 用 faux 走真实 agent loop 写 jsonl   │
│    cleanup()      → Drop 自动触发（TempDir）             │
└─────────────────────────────────────────────────────────┘
         │
         ▼ 基于
┌─────────────────────────────────────────────────────────┐
│  FauxProvider（架构级基础设施，对标 pi）                 │
│    注册成 "faux" provider，model.api=="faux" 时路由到它  │
│    FIFO 队列 + 工厂函数响应（能拿 context）              │
│    队列空 → 强制报错（loud failure，不静默通过）         │
│    走完整 EventStream → agent loop → session.jsonl       │
│    和真实 LLM 链路一模一样，但不联网、确定、免 key       │
└─────────────────────────────────────────────────────────┘
```

### 1.2 为什么基于 FauxProvider（而不是直接写 JSONL）

| 方式 | 优点 | 缺点 |
|------|------|------|
| **直接写 JSONL**（ION 现有方案） | 快 | 跳过 agent loop，回滚后"继续对话"无法 mock，不真实 |
| **FauxProvider**（本方案，对标 pi） | 走完整链路、能续轮、能断言 context | 要先实现 FauxProvider |

选 FauxProvider 的理由：你要的是"走完整链路、能回滚后继续对话"——直接写 JSONL 做不到。FauxProvider 让 seed() 变成"真的跑一轮 agent"，回滚后的新消息也能通过 faux 预设。

### 1.3 Harness 的设计原则

| 原则 | 说明 |
|------|------|
| **不调真实 LLM** | FauxProvider 按队列回放，零网络 |
| **走完整 agent 链路** | agent loop / 工具调用 / session.jsonl 写入都和真实一样 |
| **环境隔离** | 每个用例独立临时目录（TempDir），不写 `~/.ion` |
| **确定性** | 同样队列永远同样输出，无网络依赖 |
| **暴露原始对象** | 测试直接操作 session_file/faux/events，harness 不封装一层（对标 pi） |
| **loud failure** | 队列空时报错，不让测试静默通过（对标 pi） |

### 1.4 Harness 接口（给评审方看的形式）

**核心哲学（对标 pi）：harness 暴露原始对象，测试直接操作。** 不像传统测试框架那样封装一层查询方法。

每个用例都是这个套路：

```yaml
# 1. 创建 harness（自动隔离 + 注册 FauxProvider）
harness = createHarness()

# 2. 造会话——seed 走真实 agent loop，faux 预设响应
harness.seed("实现加法", [
  faux_assistant_message("fn add(a,b){a+b}")    # faux 回这个
])
harness.seed("加日志", [
  faux_assistant_message("已加 println!")
])
# → 4 条消息真实写入 session.jsonl

# 3. 操作：直接调 SessionFile 方法（harness 暴露原始对象）
entry_id = harness.session_file.find_by_text("已加 println!")
harness.session_file.branch(entry_id, name: "try-div")

# 4. 回滚后继续——仍能用 faux 续轮
harness.seed("加除法", [
  faux_assistant_message("fn div(a,b){a/b}")
])

# 5. 断言：直接查 SessionFile（harness 不封装一层）
tree = harness.session_file.get_tree()
leaf = harness.session_file.current_leaf()
hash = sha256(harness.file_path)   # 用例自己算 hash 验证 only-append

# 6. 清理（自动，Drop TempDir）
```

### 1.5 Harness 暴露的原始对象（对标 pi）

测试直接操作这些对象，harness 不重新封装：

| 对象 | 类型 | 用途 |
|------|------|------|
| `harness.session_file` | `SessionFile` | 直接调 `get_tree()`/`branch()`/`rollback()`/`current_leaf()` |
| `harness.faux` | `Arc<FauxProvider>` | `set_responses()`/`call_count()`/`pending_count()` |
| `harness.events` | `Vec<Value>` | 事件收集数组（扩展钩子触发的事件） |
| `harness.file_path` | `PathBuf` | JSONL 文件路径（用例可自己读/hash） |
| `harness.session_id` | `String` | 会话 ID |

**harness 自己只提供 4 个便捷方法：**
- `create()` / `load_from(path)` / `seed(text, responses)` / 自动 cleanup

---

## 2. 用户能看到什么（产品视角）

### 2.1 用户命令（最终通过 harness 触发等价操作）

| 命令 | 作用 | harness 等价方法 | 类比 |
|------|------|----------------|------|
| `ion --resume <sid> --branch <id> "..."` | 从某消息分叉 | `harness.branch(id)` | `git checkout -b` |
| `ion --resume <sid> --branch <id> --name X "..."` | 分叉并命名 | `harness.branch(id, name: X)` | `git branch X` |
| `ion --resume <sid> --checkout X "..."` | 切换分支 | `harness.checkout(X)` | `git checkout X` |
| `ion --resume <sid> --rollback <id> "..."` | 回滚（路径保留） | `harness.rollback(id)` | `git reset`（不丢 commit） |
| `ion --resume <sid> --rollback <id> --rollback-reason "..." "..."` | 回滚并记录原因 | `harness.rollback(id, reason: "...")` | 带备注的 reset |
| `ion --fork-from-leaf <sid>/<id> "..."` | 提取路径成新会话 | `harness.forkFromLeaf(id)` | `git worktree add` |
| `ion session tree <sid>` | 查看消息树 | `harness.tree()` | `git log --graph` |
| `ion session branches <sid>` | 列命名分支 | `harness.branches()` | `git branch` |

### 2.2 树长什么样

执行 `harness.tree()` 返回的结构，渲染出来类似：

```
msg_001 [user] "实现加法函数"
└─ msg_002 [assistant] "fn add(a,b) { a + b }"
   └─ msg_003 [user] "加日志"
      └─ msg_004 [assistant] "已加 println!..."
         ├─ msg_005 [user] "改成乘法"        ← try-mul 分支
         │  └─ msg_006 [assistant] "fn mul..."
         └─ msg_007 [user] "加除法"          ← try-div 分支 [当前 leaf]
            └─ msg_008 [assistant] "fn div..."
```

---

## 3. 验收用例分级

> **所有用例通过 `SessionTreeHarness` 执行。** 每个用例给出：前置（seed 什么）、操作（调 harness 哪个方法）、预期（断言什么）。

### P0（必须通过 — 核心功能）

#### P0.1 基础分叉：分支后原路径保留

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([
  { user: "实现加法" }, { assistant: "fn add" },
  { user: "加日志" },  { assistant: "已加" },
  { user: "改乘法" },  { assistant: "fn mul" },   # msg_005/006
])
# 当前 leaf = msg_006

# 从 msg_004 分叉
harness.branch("msg_004", name: "try-div")
harness.seed([{ user: "加除法" }, { assistant: "fn div" }])  # msg_007/008

tree = harness.tree()
```

**断言：**
- ✅ `tree` 显示 msg_004 有**两个子节点**（msg_005 和 msg_007）
- ✅ `currentLeaf()` == msg_008
- ✅ msg_005/006（旧路径）仍在 `tree` 里，未被删除
- ✅ `messagesOnPath()`（LLM 会看到的上下文）= msg_001→002→003→004→007→008，**不含 msg_005/006**

#### P0.2 回滚：被回滚的消息不丢失

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...5 轮对话...])   # msg_001..msg_010
before = harness.snapshot()       # 记录初始指纹

harness.rollback("msg_002", reason: "走错了")

after = harness.snapshot()
tree = harness.tree()
```

**断言：**
- ✅ `currentLeaf()` == msg_002（光标移回）
- ✅ `after.messageCount` == `before.messageCount`（消息一条没少）
- ✅ `tree` 里 msg_003..msg_010 全部可见
- ✅ `entriesByType("branch_summary")` 有 1 条 tombstone，内容含 "走错了"

#### P0.3 切换分支：能在分支间来回跳

**Harness 操作：**

```yaml
harness = createHarness()
# 造两个命名分支
harness.seed([...])
harness.branch("msg_004", name: "try-mul")
harness.seed([...])
harness.branch("msg_004", name: "try-div")
harness.seed([...])

# 切到 try-mul
harness.checkout("try-mul")
leaf_a = harness.currentLeaf()
harness.seed([{ user: "mul 优化" }])

# 切到 try-div
harness.checkout("try-div")
leaf_b = harness.currentLeaf()
harness.seed([{ user: "div 优化" }])

branches = harness.branches()
```

**断言：**
- ✅ checkout try-mul 后，新消息接在 mul 分支末尾
- ✅ checkout try-div 后，新消息接在 div 分支末尾
- ✅ 两个分支各自独立，`messagesOnPath()` 只含当前分支的路径
- ✅ `branches` 里 `[当前]` 标记正确切换

#### P0.4 树展示结构正确

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...])
harness.branch("msg_004", name: "A")
harness.seed([...])
harness.checkout("main")   # 或 branch 回原路径

tree = harness.tree()
```

**断言（tree 返回的 JSON 结构）：**
- ✅ 每个节点有 `id` / `role` / `content` / `children`
- ✅ 父子关系正确（msg_004 的 children 含两个分叉）
- ✅ 命名分支信息齐全（name → targetId）
- ✅ 当前 leaf 标记清晰

#### P0.5 重启后状态正确恢复

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...])
harness.branch("msg_004", name: "try-div")
harness.seed([...])   # 当前 leaf = msg_008
sid = harness.sessionId()
path = harness.filePath()

# 模拟"重启"：丢弃内存中的 harness，从文件重新加载
harness2 = loadHarnessFromFile(path)
leaf = harness2.currentLeaf()
harness2.seed([{ user: "继续" }])
```

**断言：**
- ✅ `harness2.currentLeaf()` == msg_008（正确恢复，不是文件末尾）
- ✅ 新消息接在 msg_008 之后（parentId == msg_008）
- ✅ 不需要重新 checkout

---

### P1（应该通过 — 边界与可靠性）

#### P1.1 only-append 不变量：所有操作后文件只增不减 🔴 红线

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([{ user: "m1" }, { assistant: "a1" }, { user: "m2" }, { assistant: "a2" }])
initial = harness.snapshot()    # { sha256, lineCount: 5, messageCount: 4 }

# 一系列操作
harness.branch("msg_002", name: "A")
harness.checkout("A")
harness.rollback("msg_002", reason: "test")
harness.branch("msg_002", name: "B")

final = harness.snapshot()
# 关键验证：前 initial.lineCount 行的内容是否一字未改
prefixHash = harness.snapshotPrefix(initial.lineCount)
```

**断言：**
- ✅ `final.lineCount > initial.lineCount`（行数严格递增）
- ✅ `final.messageCount >= initial.messageCount`（消息不丢）
- ✅ **`prefixHash == initial.sha256`**（前 N 行内容完全不变——这是 only-append 的核心证明）

#### P1.2 fork-from-leaf 不修改源文件

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...])
harness.branch("msg_004", name: "X")
harness.seed([...])

sourceHash = harness.snapshot().sha256

newHarness = harness.forkFromLeaf("msg_004")
afterHash = harness.snapshot().sha256   # 源 harness 的指纹
```

**断言：**
- ✅ `afterHash == sourceHash`（源文件零修改）
- ✅ `newHarness` 是独立会话，有自己的 sessionId
- ✅ `newHarness` 的 header 记录了源会话作为 parent

#### P1.3 回滚的可逆性

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...5 轮...])   # leaf = msg_010
harness.rollback("msg_005", reason: "重来")   # 回滚
harness.branch("msg_010")    # 再切回去（撤销回滚）
harness.seed([{ user: "恢复" }])
```

**断言：**
- ✅ 最终 `currentLeaf()` 在 msg_010 的后续
- ✅ msg_005..msg_010 全程没丢（`snapshot.messageCount` 持续递增）
- ✅ 文件里有完整的"回滚 + 恢复"记录

#### P1.4 Agent 自主分叉（FauxProvider 驱动 host 模式）

> 此用例走完整 host 引擎，用 FauxProvider 预设 Agent 会调用 `branch_session` 工具。

**Harness 操作：**

```yaml
harness = createHarness(hostMode: true)
harness.seed([
  { user: "实现排序，快排不行换归并" },
  # FauxProvider 预设：Agent 第一轮就调 branch_session
  { faux: { tool_call: { name: "branch_session", input: { from_entry: "msg_002", name: "plan-b" } } } },
  # 工具执行后，Agent 第二轮继续
  { faux: { text: "已切到 plan-b 分支，继续工作" } },
])

tree = harness.tree()
events = harness.events()
```

**断言：**
- ✅ Agent 调用了 `branch_session` 工具（events 里有 tool_call）
- ✅ 树显示两条分支
- ✅ Agent 在新分支上继续（后续消息 parentId 接新 leaf）

#### P1.5 命名冲突处理

**Harness 操作：**

```yaml
harness = createHarness()
harness.seed([...])
harness.branch("msg_002", name: "dup")
result = harness.branch("msg_003", name: "dup")   # 重名
```

**断言：**
- ✅ `result.success == false`，报错"分支名 dup 已存在"
- ✅ 第二个分支**未创建**（`branches()` 仍只有 1 个 dup）
- ✅ 或按实现策略自动改名（`dup-2`），需在文档明确

---

### XFail（预期失败 — 已知限制）

| # | 用例 | 为什么预期失败 | 何时修复 |
|---|------|--------------|---------|
| X1 | `harness.branch("msg_before_compaction")` | 分支穿越 compaction 点会丢摘要，拒绝执行并报错提示用 forkFromLeaf | 后续版本支持"分支带 compaction 截断" |
| X2 | `harness.branch("nonexistent_id")` | entry 不存在，报错退出 | 预期行为（非 bug） |
| X3 | `harness.checkout("nonexistent_name")` | 分支名不存在，报错列出可用名 | 预期行为 |
| X4 | 超长会话（10000+ 消息）的 `tree()` 性能 | 明显变慢 | 后续加缓存/分页 |
| X5 | 跨会话的任意 fork 血统追溯 | 只支持 forkFromLeaf 的血统记录 | 后续做完整跨文件树 |

---

## 4. 验收判定标准

| 条件 | 判定 |
|------|------|
| **P0.1 - P0.5 全部通过** | ✅ 核心功能可用，可发布 |
| **P1.1 - P1.5 通过率 ≥ 80%** | ✅ 可靠性合格 |
| **P1.1（only-append）必须 100% 通过** | 🔴 **绝对红线**——失败即不可发布 |
| **XFail 用例确实失败** | ✅ 已知限制受控（不算缺陷） |
| **P0 任一失败** | ❌ 不可发布，必须修复 |

**特别强调 P1.1：** 任何操作导致旧记录被修改或删除（`prefixHash != initial.sha256`），都视为严重缺陷，无论其他用例是否通过。

---

## 5. 前置条件

| # | 条件 | 验证 |
|---|------|------|
| 1 | ion 项目能编译 | `cargo build --bin ion --bin ion-worker` 无错误 |
| 2 | FauxProvider 已实现 | `ion --faux-reply "test" --model faux/test "hi"` 能跑通 |
| 3 | harness 二进制能跑 | `cargo test --test session_tree_harness -- --list` 列出用例 |
| 4 | 无需真实 LLM API key | 全部走 FauxProvider |
| 5 | 无需网络 | FauxProvider 是纯内存回放 |
| 6 | host 模式用例（P1.4）走 FauxProvider | 子 worker 通过 `ION_FAUX_SCRIPT` 继承 |

---

## 6. 验收 checklist（评审方逐项打勾）

### P0 核心功能

- [ ] **P0.1** 分叉后原路径保留，树结构正确
- [ ] **P0.2** 回滚后被回滚消息不丢失，有 tombstone 标记
- [ ] **P0.3** 分支切换正确，分支间互不污染
- [ ] **P0.4** `tree()` 树结构正确（父子关系、命名分支、当前 leaf）
- [ ] **P0.5** 重启（重新加载文件）后 current leaf 正确恢复

### P1 边界与可靠性

- [ ] **P1.1** only-append 不变量：`prefixHash == initial.sha256` 🔴
- [ ] **P1.2** forkFromLeaf 源文件零修改
- [ ] **P1.3** 回滚可逆，数据零丢失
- [ ] **P1.4** Agent 自主分叉正常工作
- [ ] **P1.5** 命名冲突有合理处理

### XFail 已知限制

- [ ] **X1** 分叉到 compaction 前被正确拒绝
- [ ] **X2** 不存在的 entry id 报错
- [ ] **X3** 不存在的分支名报错

---

## 7. 与其他功能的关系

| 关联功能 | 关系 | 验收点 |
|---------|------|--------|
| **Compaction（会话压缩）** | 分叉不能穿越 compaction 点 | X1 |
| **Memory 扩展** | 分叉后 memory 注入应跟随当前 leaf 路径 | P1.4（观察 events） |
| **Worktree 隔离** | 本期不做联动，分叉不创建 worktree | 不验收 |
| **Session 导出（--export）** | 导出应包含完整树，不止当前 leaf 路径 | P1.2 延伸 |

---

## 8. 不在本期范围

明确**不做**的，避免评审方误判为缺陷：

- ❌ 跨会话的完整血统树（只做文件内分支）
- ❌ 分支的 LLM 自动摘要（tombstone 是纯文本）
- ❌ 分支与 worktree 联动
- ❌ 树的图形化 UI（harness 只返回 JSON 结构）
- ❌ 树导出为 dot/mermaid 格式
- ❌ 被废弃路径的自动 GC

---

## 9. 名词解释（给非开发读者）

| 名词 | 含义 |
|------|------|
| **harness** | 测试脚手架，封装"造会话/操作/检查"，所有用例通过它执行 |
| **FauxProvider** | 架构级 LLM Mock，注册成 "faux" provider，按预设脚本回放响应，走完整 agent 链路但不联网 |
| **seed** | 往会话塞预设消息——通过 FauxProvider 走真实 agent loop，让消息沉淀到 session.jsonl |
| **session** | 一次会话，一个文件，记录所有对话 |
| **entry** | 会话里的一条记录（一条消息、一次分支操作等） |
| **leaf / 光标** | 标记"当前活跃在哪条消息"的指针 |
| **branch（分叉）** | 从某条消息长出新枝，原枝保留 |
| **rollback（回滚）** | 把光标移回某条旧消息，被跳过的路径保留 |
| **checkout（切换）** | 跳到另一个命名分支 |
| **fork-from-leaf** | 把某条路径提取成新的独立会话 |
| **tombstone** | 回滚时追加的标记，说明"这段路径被主动废弃了" |
| **only-append** | 文件只追加新记录，永不修改/删除旧记录的核心约束 |
| **snapshot（指纹）** | 对文件拍照（hash + 行数），用于验证 only-append |
| **compaction** | 会话压缩，把老消息汇总成摘要以节省 token |
| **P0/P1/XFail** | 用例优先级：必须通过 / 应该通过 / 预期失败 |
