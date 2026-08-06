# ION 项目状态

> **状态：开发中** — 未上线，无需兼容旧数据。最近盘点：2026-08-06。
>
> 本文件是**当前快照**（功能清单 + 测试统计 + 路线图）。历史 commit 看git log`，详细设计看 `docs/design/`。

---

## 一、规模快照（2026-08-06 实测）

| 维度 | 数值 |
|------|------|
| Rust 代码总行数 | **99,682**（src 82,912 + ion-provider ~5k + 其他） |
| lib 测试 | **1013 passed / 2 failed**（2 个 hooks 测试逻辑缺陷，非产品 bug，待修） |
| 文档 | 111 篇 .md |
| 设计文档 | `docs/design/` 40+ 篇 |

---

## 二、已完成功能清单

> 仅列**已落地**的功能。设计/排期中的看路线图。

### 核心内核

- **CLI** — 45+ 参数（对齐 pi 41 核心）。单一二进制 `ion`，`--mode rpc` 进 worker 模式
- **Provider 抽象** — `ion-provider` 独立 crate，OpenAI SSE + tool_calls，4 provider + 9 协议（含 Azure/Codex/Vertex）
- **Agent 循环** — 内外两层 + 约 45 个 Extension trait 方法 + 23 已接入
- **27 个内置工具** — read/write/edit/bash/grep/find/ls/calculator/echo + 7 Git + spawn/send/resume/await/channel_send/kill + global_memory_search/save + branch_session + remote tool
- **会话管理** — JSONL v3 + 实时索引 + fork/continue/resume + cwd-hash 分组
- **三场景引擎** — 场景 1 直接执行 / 场景 2 临时 host / 场景 3 常驻 host
- **多智能体编排** — 7 个工具（spawn_worker/resume/send/await/kill/channel_send + send_to_session），同步异步全覆盖

### 扩展系统

- **WASM 扩展** — 热更新 + 4 维数据存储 + 27 host functions + 36 生命周期钩子
- **Extension Host API** — `ctx.fs` 统一文件访问 + `safe_join` 路径逃逸防护
- **5 类 hooks 系统** — command/http/prompt/agent/mcp_tool（2975 行，热重载）
- **MCP 系统** — rmcp 1.x + host→worker 工具发现 + 重连监控 + resources/prompts
- **Memory V0.2** — 单例扩展 + SQLite/FTS5 + 跨项目检索 + 中文 LIKE fallback
- **File Snapshot** — 双路快照（object + tree）+ zstd 压缩 + 三级 GC + restore/approval
- **Goal Supervisor** — `on_gate_check` 证据驱动目标闭环 + 6 道防线 + 趋势分析 + goal_refine/diagnose
- **Goal Evolver** — 日志分析进化（3 维度：deadloop/model/context）→ Issue 计划
- **LSP Extension** — 5 语言诊断（Rust/TS/Python/Go/HTML，基于编译器 JSON 输出，非 rust-analyzer）
- **Rules Engine** — `.ion/rules/*.md` frontmatter glob 匹配 → 注入 XML
- **Learning Extension** — Secret Detector + 会话分析 + Skill Distillation（LLM 提炼）
- **Monitor Extension v2** — 定时监控→LLM 触发 + self-healing pipeline
- **Plan 工具** — 内置 6 工具 + strict_mode 强制审批
- **PermissionProfile** — 5 模式（permissive/readonly/standard/strict/autopilot）
- **Stored-Decision 权限记忆** — "always allow" 持久化

### 其他

- **Session Tree** — 文件内分支 + leaf 指针 + only-append 回滚
- **Compaction** — 分批并发 + LLM summarizer + emergency fallback
- **Message Retrieval** — 9 接口 + 分页/视点/过滤/turn 聚合
- **Record/Replay** — LLM 决策录制回放（复用 FauxProvider）
- **FauxProvider** — 架构级 LLM Mock（FIFO 队列 + 工厂响应 + 流式分块）
- **Worker 崩溃恢复** — stderr 捕获 + exit code + Dead 保留 + 父通知
- **HTML Export** — pi 模板 + agent/model/banner + tools 列表
- **Apple Container 后端** — 真隔离 Linux VM，同端口并行
- **A→B 自进化** — A 调度 B 改代码 + CI + 合并 + PR（14 脚本，24 agent 模板）

---

## 三、A→B 自进化（核心特色）

**铁律：A 只调度，B 改代码。** A 永远通过 `container exec B ion --agent developer` 让 B 在隔离环境动手。详见 [docs/design/SELF_EVOLUTION.md](./design/SELF_EVOLUTION.md) + [docs/design/EVOLVER_LESSONS_LEARNED.md](./design/EVOLVER_LESSONS_LEARNED.md)。

**累计验证**：14 个 A→B 闭环任务，39 个测试全过，0 个 U+FFFD 残留。

**6 道守门机制**：① U+FFFD 守门 ② Cargo.toml 守门 ③ Reviewer agent ④ cargo build ⑤ cargo test --lib ⑥ cargo clippy。

**关键脚本**（`scripts/evolve*.sh`，活跃；已归档的在 `scripts/archive/`）：
- `evolve.sh` — 启 container + volume cache 编译
- `evolve_self.sh` — 串行批量（B 改 ION 自己源码）
- `evolve_pr.sh` — B 改代码 → GitHub PR → merge
- `evolve_verify.sh` — 独立 CI 验证
- `init-evolve-container.sh` — container 初始化（装 Rust + 复制 ion binary）
- `auto_evolve_local.sh` — 本地化 A→B 自循环（不用 container）

---

## 四、测试统计（2026-08-06 实测）

| 套件 | 数量 | 覆盖 |
|------|------|------|
| **lib tests** | **1013 passed / 2 failed** | 全部核心逻辑（含 2 个 hooks 测试逻辑缺陷，待修） |
| unit_rpc_test | 20 | RPC 协议 U1-U20 |
| CI 脚本 | 30+ 个 `tests/*_ci.sh` | CLI 外部验证（MCP/hooks/extensions/snapshot/goal/memory 等） |

**2 个失败测试详情**：
- `hooks::extension::tests::test_has_hooks_returns_false_for_nonexistent_dir`
- `hooks::extension::tests::test_new_preserves_project_dir_usable_for_has_hooks`
- 根因：`has_hooks()` 实现合并全局 `~/.ion/hooks.json`，但测试假设只查 project_dir。**测试逻辑错误，非产品 bug**。
- 修法：测试在临时 HOME 下跑（隔离全局 config），或改 `has_hooks` 语义只查 project_dir。

---

## 五、路线图

**P0（当前）**：~~CommandGuard + 权限引擎~~ ✅ 完成
**P1（Runtime 抽象）**：✅ 完成（Local/Sandbox/Remote/AppleContainer + BackendRegistry 路由层）
**P2（路径权限）**：✅ 完成（CommandGuard 三模式 + PermissionRule 热重载）
**P3（UI 对接）**：✅ 完成（subscribe --ui + ui_respond + audit.jsonl）
**P4（扩展生态）**：✅ 完成（ExtensionApi + WASM 钩子 + emit 自定义事件）
**P5（包管理）**：✅ 完成（`ion extension install/remove/list`）

**待办（无优先级）**：
- 修 2 个 hooks 失败测试
- scripts/ 目录归档（evolve 系列 14 个 → 留核心 3-4 个）
- 105 个 TODO/FIXME 筛查（多数是测试 fixture，真技术债约 20-30 处）

---

## 六、推荐模型配置

| 用途 | 模型 | Provider | 说明 |
|------|------|----------|------|
| **B 改代码（主力）** | `glm-5.2` | `zai` | UTF-8 稳定（无 U+FFFD）、推理强 |
| **快速测试** | `deepseek-v4-flash` | `opencode` | 便宜快，适合 CI |
| Avoid | claude-opus / gpt-4o | — | 昂贵 |

配置示例见 [AGENTS.md §模型配置](../AGENTS.md)。
