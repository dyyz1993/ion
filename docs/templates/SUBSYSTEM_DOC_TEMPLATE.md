# 子系统现状文档模板（SUBSYSTEM_DOC_TEMPLATE）

> **状态：已完成** — 模板定稿；首个使用样本见 [docs/subsystems/session-index.md](../subsystems/session-index.md)。

来源：magpie（yetone/magpie）`docs/subsystems/` 12 篇文档通读归纳，ION 本地化（命令改 cargo、锚点改 file:line + 测试名 + AGENTS.md 修复批记录）。

**定位：写现状，不写教程、不写提案**——描述子系统**当前**的行为、契约和验证方式，供没有上下文的维护者 / AI agent 改代码前对照。

---

## 何时使用 / 与其他模板的分工

| 体裁 | 模板 | 回答的问题 |
|------|------|-----------|
| 设计提案（配置/主流程/接口规格/CLI 测试） | [DESIGN_TEMPLATE](./DESIGN_TEMPLATE.md) | 这个功能**打算**怎么做 |
| **现状文档（本模板）** | — | 这个子系统**现在**真实怎么跑、契约是什么、怎么验证 |
| CLI 验证用例 | [CLI_TEST_TEMPLATE](./CLI_TEST_TEMPLATE.md) | 怎么从命令行验证一个功能 |
| 测试规格 / pi 对齐 / 扩展手册 | 其余三模板 | — |

规则：

- 功能**开发期**用 DESIGN_TEMPLATE；功能**实现并稳定后**，核心子系统补一份现状文档，放 `docs/subsystems/<子系统>.md`，与设计文档**互链不复制**（一个事实只写一处，设计文档链接过来）。
- **不必给每个模块都写**——只写最常被改、最缺上下文的核心子系统（索引、审批总线、恢复/重派、RPC 路由这类）。magpie 的经验：核心才有，全量写是负担。
- 触发时机：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

---

## 骨架

```markdown
# <子系统名>[：副标题]

> **状态：现状快照** — 基准 commit <sha8>（<日期>，验证情况见 Verification）。

（无标题引言，1–3 段）

## Responsibilities and sources of truth   ← 三列表格
## Runtime path                            ← 编号列表
## Constraints and failure behavior        ← bullet 不变式列表
## Verification                            ← ```sh 命令块 + 收尾说明

（可选）## <专题章节> × N                   ← 复杂特性升级成独立 H2
```

- 标题层级最多到 H3，禁止更深；全文层级扁平。
- ION 的状态标注用「现状快照 + 基准 commit」——现状文档随代码变，**改契约的 commit 必须同步更新本文并刷新基准 commit**。

## 标题

名词短语，一行；可带冒号副标题限定范围（`# Session index: 会话热字段目录与跨进程读写协议`）。

## 引言（无标题，直接开写）

三句话定生死，缺一不可：

1. **一句话定义**：这个子系统是什么、物理入口在哪（文件/进程/端口）。
2. **一句话划界**：本页讲什么、不讲什么（相邻话题点名推走）。
3. **相邻文档交叉引用**：相邻子系统文档、对应设计文档、AGENTS.md 相关原则各带链接。

## Responsibilities and sources of truth

三列表格，一部件一行：

```markdown
| Part | Responsibility | Source |
| --- | --- | --- |
| <部件名> | <它负责什么，1–3 句> | `SymbolName` in [`src/xxx.rs`](../../src/xxx.rs) |
```

- **Source 列必填**：反引号符号名 + `in` + 相对路径 markdown 链接（从 `docs/subsystems/` 出发是 `../../src/...`）；一个部件可引多个符号/文件。
- 部件名用名词（`Boot reconcile`、`Tombstones`）；单句超过 5 句拆成专题章节，别塞表格。
- 表格后可跟一段"谁是 source of truth"的裁决说明（如：正文在 JSONL，索引只是可重建的目录）。

## Runtime path

编号列表，按调用顺序走完整条链路，每步以**加粗动词**开头（`**Create.**` / `**Degrade.**`）：

- 每步 = 一个环节的完整行为，含正常路径 + 分支去向（"例外：`list_sessions` 读 registry，不读索引"）。
- 步骤间时间/触发关系写清楚（"启动时"、"随事件"、"write_txn 内"）。
- 每步结尾用 `Covered by \`TestName\`` 落到测试（有真实测试才写，禁止"有相应测试覆盖"这种空话）。

## Constraints and failure behavior

bullet 列表，一条一个不变式，密度照这个标准：

> - 所有写走 `write_txn` 的 flock 独占锁；绕过事务的独立 load→modify→save 是 last-write-wins 互踩。

- 失败分类要窄：不同错误分类别合并（SaveFailed / QuarantinedCorrupt / LockDegraded 各写各的）。
- **读失败 ≠ 空值**：任何"读不到/解析不了"的语义必须写明是 unknown/quarantined 还是 empty。
- 数值常量内联默认值：`` 墓碑超 512 (512) 全清 ``。
- 面向修复者的约束可单列 `### Constraints for fixes`，此时允许祈使句（全文唯一可出现第二人称的地方）。

## Verification

```sh 代码块 + 收尾段。命令只有 ION 的两种形态（测试名用空格分隔做多选过滤，cargo 过滤器是子串匹配不是正则）：

```sh
cargo test --lib -- test_get_children test_has_children ...
cargo test --test <integration_target>
bash tests/<feature>_ci.sh
```

- 命令与正文各步 `Covered by` 的测试名一一对应。
- 收尾段写**环境前提 + 实测口径**：哪些跑过（commit + 日期 + 通过数）、哪些 not run（**不许省略**）、脚本是否碰真实 `$HOME` 下文件（备份/还原机制）、全量回归注意（如 `--test-threads=2` 惯例）。

## 专题章节（骨架之外）

某特性复杂到表格行装不下时升级成独立 H2，**命名用现象而非术语**（`## A session that outlived its index entry`）。节内结构：是什么 → 运行路径 → Before（旧行为与事故，挂 AGENTS.md 修复批或 commit）→ `Covered by` 测试。一节 100–350 词。

---

## 句子级文风守则

| 规则 | 写 | 不写 |
| --- | --- | --- |
| 时态人称 | 一般现在时、第三人称、零人称 | 你 / 我们 / 过去时叙述（"Before, …" 除外） |
| 主句式 | 一句一规则：`<规则> (<symbol>) (<commit/#issue>)` | 分散多段的模糊描述 |
| 强调 | 长句间插绝对短句："索引故障从不阻塞会话功能。" | 形容词堆砌（"非常健壮"） |
| 不变式 | "X, never Y" / "意味着 unknown, never empty" | "通常"、"一般来说" |
| 修复记录 | "Before, <旧行为与事故>（AGENTS.md 修复批 N / commit xxx）。现在 …" | 只写改了什么不写为什么 |
| 收尾 | `` Covered by `TestName` `` | "有测试覆盖" |
| 代码 | 全部内联反引号；**全文唯一代码块是 Verification 的 sh** | 贴实现代码、贴 JSON |

## 证据锚点（每条关键事实至少挂一类）

| 锚点 | 格式 |
| --- | --- |
| 测试 | 内联 `` `TestName` `` / `tests/xxx_ci.sh` |
| 实测 | 基准 commit + 日期 + 通过数（"lib 8/8，2026-10-14"） |
| 事故/动机 | （AGENTS.md 修复批 N 记录 / commit `<sha8>`） |
| 上游依据 | pi 源码路径（对齐类事实才用） |

## 维护规则（同 commit 强制）

1. 改动触及**职责、接口、状态转换、运行路径、兼容性规则、失败行为**任一项 → **同一个 commit** 更新所属现状文档并刷新基准 commit；纯内部重构不用。
2. 一个事实只写在一处，其他地方链接过来。
3. 链接源码文件和具名符号，不复制会频繁变化的实现细节。
4. 评审/自查时逐个检查文档链接可解析；声称的行为要有测试或实测证据；没跑的检查写明 not run。

## 自检清单

- [ ] 引言三件套齐：定义 / 划界 / 相邻文档链接
- [ ] 职责表三列全，Source 列全是可解析的相对链接 + 符号名
- [ ] Runtime path 按调用顺序可走通，步骤动词开头，例外分支点名
- [ ] Constraints 每条一个不变式，失败分类没有合并
- [ ] 数值都带默认值，读失败都 ≠ 空值
- [ ] Verification 命令可独立运行，与正文测试名对应；not run 的写明
- [ ] 全文无第二人称（Constraints for fixes 除外）
- [ ] 唯一代码块是 Verification
- [ ] 每条关键行为挂了测试/实测/事故至少一类锚点
- [ ] 表格单元格没有超过 5 句（超了拆专题节）
- [ ] 状态行带基准 commit，本次变更已刷新

---

## 配套实践：错题本

原模板配套的 LESSONS.md（错题本 + 三振转正）**已引进**：见 [docs/LESSONS.md](../LESSONS.md)——复盘已合并工作的返工/验证太薄项，祈使句规则 + 证据（SHA/测试名/修复批），同一教训 ≥3 个不同日期搬进 [AGENTS.md](../../AGENTS.md) 成为评审强制项。复盘手动启动，不做 cron。其余按 magpie 自己的最小落地路线建议：**只给核心子系统写现状文档，不搞全量覆盖**。
