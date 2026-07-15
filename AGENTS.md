# ION — AI Agent Orchestration Platform

> 一个用 Rust 实现的 AI Agent 编排平台，对齐 pi (pi-coding-agent) 的全部能力。

## ⚠️ 术语规范：统一使用 Extension，禁止使用 Plugin

**本项目所有可扩展能力统称为 Extension。禁止使用 "plugin"、"插件" 这两个词。**

### 两类 Extension（API 完全一致，31 个生命周期钩子）

| 类型 | 加载方式 | 可关闭 | 例子 |
|------|---------|--------|------|
| **内置 Extension** | Rust 编译进内核 | ✅ config.json `extensions.X.enabled = false` | Memory / Bash / Streaming |
| **运行时 Extension** | WASM 动态加载 (`.wasm`) | ✅ 不加载即可 | todo / stock / plan / 任何第三方 |

两者唯一的区别是"代码住哪"——编译进二进制 vs 运行时从文件加载。拿到的 `Extension` trait 接口、钩子、数据访问权限完全相同。

### WASM Extension ABI 符号约定

WASM 模块导出的 C 函数必须使用 `extension_` 前缀：
- `extension_version()` / `extension_init()` / `extension_execute_tool(...)`
- `extension_on_input(...)` / `extension_on_context(...)` / `extension_on_system_prompt(...)` 等约 30 个生命周期钩子 + 单例管理 + RPC + Gate
- `extension_on_rpc(...)` — extension_rpc 入口

**不要使用 `plugin_*` 前缀，已废弃。**

### 检查清单

写代码/文档时自查：
- ❌ `PluginRegistry` → ✅ `ExtensionRegistry` / `Registry`
- ❌ `plugin_rpc` → ✅ `extension_rpc`
- ❌ `--plugin <name>` → ✅ `--extension <name>`
- ❌ `PluginEvent` / `PluginEventBus` → ✅ `ExtensionEvent` / `ExtensionEventBus`
- ❌ `emit_plugin_event` → ✅ `emit_extension_event`
- ❌ "插件" → ✅ "扩展"
- ❌ `plugin_init` / `plugin_version` (WASM ABI) → ✅ `extension_init` / `extension_version`

---

## 内核 vs 扩展：功能设计指导方针

当讨论一个新功能放在哪时，按这个顺序思考：

1. **这个功能是基础设施还是策略？**
   - 基础设施（进程管理、通信、文件系统、安全模型）→ **内核**
   - 策略/行为定制（Agent 怎么回答、用什么语气、审查规则）→ **扩展**

2. **如果答案是扩展，先检查内核是否提供了足够的扩展点。**
   - 缺钩子？加钩子（Extension trait 加方法）
   - 缺数据？加数据结构
   - 缺通信能力？补 Manager command 管道
   - **永远不要因为内核不满足条件就把功能推到扩展端。先补齐内核，再让扩展用。**

3. **如果答案是内核，直接做。**

4. **如果一个能力可能被多个扩展共用，它应该在内核实现，通过 ExtensionApi 暴露给扩展。**
   - 比如 `create_worker`、`channel_send`、`emit` 都是内核能力，不是某个扩展的私有逻辑
   - 每个扩展拿到的是 `ExtensionApi`（内核给的把手），不是自己造轮子
   - 判断标准：**如果两个无关的扩展都想做同一件事，这件事就该进内核**

5. **例外：如果功能涉及用户自定义逻辑、运行时热加载、第三方集成，优先考虑做成扩展钩子 + 默认扩展实现**——内核提供钩子和默认值，扩展覆盖行为。

**一句话：内核要足够强大，让扩展只做策略层的事。内核提供能力，扩展编排能力。**

## 参考实现：pi (pi-coding-agent)

ION 对标 pi 的全部能力。遇到不确定的设计决策时：

1. **先查 pi 源码**：
   - pi 源码位置：`/Users/xuyingzhou/Project/temporary/pi-momo-fork/`
   - 模型定义（1039 个模型）：`packages/ai/src/models.generated.ts`
   - Provider 协议实现：`packages/ai/src/providers/`
   - RPC 协议：`packages/rpc/`
   - 会话存储 JSONL：`packages/session/`

2. **pi 的模型配置**（参考 `~/.pi/agent/models.json`）：
   - 34 个 Provider，支持 9 种 API 协议（`openai-completions` / `anthropic-messages` / `google-generative-ai` / `openai-responses` / `bedrock-converse-stream` 等）
   - 模型字段：`id`, `name`, `api`, `provider`, `baseUrl`, `reasoning`, `thinkingLevelMap`, `input`, `cost`, `contextWindow`, `maxTokens`
   - ION 当前实现：从 `~/.ion/models.json` 或 `~/.pi/agent/models.json` 加载（`ion-provider/src/registry.rs`）

3. **摇摆不定的决策**：
   - 方法签名、字段命名、协议格式 → 参考 pi 的实现
   - 行为预期不清楚 → 看 pi 怎么做的
   - pi 没有的（如 worktree 隔离、多 Worker 团队）→ ION 原创设计，记录在 [docs/design/TEAM_ORCHESTRATION.md](./docs/design/TEAM_ORCHESTRATION.md)

## 文档规范

### 根目录整洁原则

**根目录只保留** `AGENTS.md` + `README.md` + 标准配置文件（Cargo.toml / Makefile / .gitignore / Cargo.lock）。

所有设计文档、指南、模板、测试文档必须放到 `docs/` 子目录：

```
docs/
├── README.md                  ← 文档总导航
├── templates/                  ← 5 个文档模板
├── guides/                     ← 使用指南
├── design/                     ← 功能设计文档
├── testing/                    ← 测试用例
└── archive/                    ← 已归档（被合并/被替代）
```

详细导航见 [docs/README.md](./docs/README.md)。

### 文档状态标注

每个文档开头必须标注状态：
- **已完成** — 功能已实现并通过验证
- **已验证** — 功能已实现并经过真实场景测试
- **开发中** — 正在实现
- **暂不开发** — 已设计但未排期
- **待定** — 有想法但未形成设计

格式：

```markdown
# 文档标题

> **状态：已验证** — 一句话说明当前进度。
```

### 测试验证规范（每个功能必须遵守）

每个功能（无论是新建还是重构）**必须**配套以下两层验证，缺一不可：

| 层 | 要求 | 机制 | 何时用 |
|----|------|------|--------|
| **Harness 验证** | 必须有 | FauxProvider Factory 集成测试（`cargo test --test`）| 验证 agent 真实行为（工具调用、hook 触发、多轮交互），不调真 LLM |
| **真实 case** | 必须补 | `#[ignore]` e2e 测试 + `ION_E2E=1` 环境变量 | 最后补，验证真实 LLM 场景 |

**Harness 优先原则**：先用 FauxProvider 写 harness 测试把闭环跑通（零 API 成本、确定性），验证通过后再补真实 case。

**FauxProvider 的两种模式**：
- **Static**（`ION_FAUX_REPLY` / `ION_FAUX_SCRIPT`）：固定响应序列，适合 CLI 冒烟、RPC 连通性测试
- **Factory**（Rust 闭包，`FauxResponseStep::Factory`）：根据 context 动态返回，适合审批/多轮交互等需要"根据上下文决定行为"的场景

**自查清单**（功能完成前必查）：
1. ✅ 有 harness 测试吗？（FauxProvider 驱动，不调真 LLM）
2. ✅ Factory 用在需要动态分支的场景了吗？（审批、多轮交互必须用 Factory）
3. ✅ 有 `#[ignore]` 真实 case 吗？（标 `ION_E2E=1` 触发）
4. ✅ 测试文档里有 harness 章节 + 真实 case 章节吗？

**真实 LLM 测试推荐模型**：写真实 case（`ION_E2E=1`）或手动验证时，**优先用 `deepseek-v4-flash`**（便宜、快速、够用），不要用昂贵的旗舰模型。

```bash
# 手动快速验证（非交互，跑完即退）
ion -p "帮我创建一个 hello.txt" --provider opencode --model deepseek-v4-flash

# CI 真实 LLM 测试（Group L 用 default config 的 glm-4.7，也可临时切）
ION_E2E=1 bash tests/file_snapshot_ci.sh
```

> 避免用 claude-opus / gpt-4o 等昂贵模型做日常测试——成本高且没必要。`deepseek-v4-flash` 足以验证工具调用、审批闭环、多轮交互等场景。

### UI 交互架构规范（每个对外功能必须遵守）

ION 支持多终端（CLI / Web UI / IDE 插件）同时连接同一个 host。每个对外功能（审批、回滚、文件快照等）**必须**同时提供以下三种能力，缺一不可：

| 能力 | 要求 | 实现方式 |
|------|------|---------|
| **被动通知（Push）** | 状态变化时主动推送事件，UI 不需要轮询 | Worker stdout → Manager event-pump → EventBus broadcast → CLI `subscribe` |
| **多窗口实时同步** | 一个终端的操作，其他终端自动刷新 | 同一个 EventBus broadcast，所有 subscriber 都收到 |
| **数据拉取（Pull）** | 新连接/刷新时能获取当前完整状态 | RPC 查询接口（如 `review_pending` / `review_approvals`） |

**三能力缺一不可的原因**：
- 只有 Push 没 Pull → 新终端连上时看不到已有状态（空白）
- 只有 Pull 没 Push → 用户必须手动刷新，体验差且多窗口不同步
- Push + Pull 但没同步 → 多终端看到不一致的状态

**自查清单**（功能完成前必查）：
1. ✅ 状态变化时有推送事件吗？（stdout JSON → Manager 转发 → subscribe）
2. ✅ 有 RPC 拉取接口吗？（新终端能获取当前状态）
3. ✅ 推送事件的 customType 统一了吗？（如 `ApprovalRequest` / `ApprovalResolved` / `ApprovalReset`）
4. ✅ 事件 data 包含足够信息让 UI 渲染吗？（文件列表、diff 摘要、操作结果）

**推送事件模式（仿 BashExtension）**：
```rust
// Worker Extension 通过 stdout 输出事件 JSON
// 注意：必须包 "type":"event" 外壳，否则 Manager 路由不转发
fn emit_event(custom_type: &str, data: &serde_json::Value) {
    let msg = serde_json::json!({
        "type": "event",
        "event": {
            "type": "extension_event",
            "extension": "<extension_name>",
            "customType": custom_type,
            "visibility": "llm_and_ui",
            "data": data,
        },
    });
    println!("{}", serde_json::to_string(&msg).unwrap_or_default());
}
```

**事件转发链路**：
```
Worker Extension (stdout JSON)
    ↓ Manager stdout-reader（识别 "type":"event"）
Manager event-pump（重建 ExtensionEvent）
    ↓ ExtensionEventBus.broadcast()
CLI subscribe / Web UI / IDE（所有 subscriber 都收到）
```

### 模板触发时机（写新文档前必读）

写新文档前**必须先查模板**。5 个模板对应 5 种触发场景：

| 触发场景 | 用哪个模板 | 模板路径 |
|---------|----------|---------|
| 启动新功能开发、或对某个子系统做完整设计 | **DESIGN_TEMPLATE** | [docs/templates/DESIGN_TEMPLATE.md](./docs/templates/DESIGN_TEMPLATE.md) |
| 功能完成需要写 CLI 验证用例（Group A/B/C/D 格式 + 完整请求/响应 JSON） | **CLI_TEST_TEMPLATE** | [docs/templates/CLI_TEST_TEMPLATE.md](./docs/templates/CLI_TEST_TEMPLATE.md) |
| 功能需要外部评审 / 给 QA 的验收规格（P0/P1/XFail 分级） | **TEST_SPEC_TEMPLATE** | [docs/templates/TEST_SPEC_TEMPLATE.md](./docs/templates/TEST_SPEC_TEMPLATE.md) |
| 调研 pi 某项能力并规划对齐方案 | **PI_ALIGNMENT_TEMPLATE** | [docs/templates/PI_ALIGNMENT_TEMPLATE.md](./docs/templates/PI_ALIGNMENT_TEMPLATE.md) |
| 写新的 WASM 扩展手册 | **EXTENSION_MANUAL_TEMPLATE** | [docs/templates/EXTENSION_MANUAL_TEMPLATE.md](./docs/templates/EXTENSION_MANUAL_TEMPLATE.md) |

**写文档前的自查清单**：
1. ✅ 这个文档属于哪个子目录？（design / guides / testing / templates）
2. ✅ 该用哪个模板？
3. ✅ 状态标注写了吗？
4. ✅ 术语规范：用 "extension" 不用 "plugin" / "插件"？
5. ✅ 同主题是否已有文档？（避免新增重复文档，应该合并到已有；旧文档归档到 `docs/archive/`）

### 形成新功能时的文档操作规范（必读）

每开始一个新功能或扩展功能时，**必须按以下顺序操作**：

**第一步：判断内核还是扩展**

按 [内核 vs 扩展方针](#内核-vs-扩展功能设计指导方针) 判断：
- 基础设施（进程/通信/存储/安全/模型选择）→ **内核** → 文档放 `docs/design/`
- 策略/行为定制（回答风格/审查规则/工具）→ **扩展** → 文档放 `{extension}/MANUAL.md`
- 两者都可能用到的能力 → **内核实现 + 扩展消费** → 设计文档放 `docs/design/`，扩展手册放 `{extension}/MANUAL.md`

**第二步：先查有没有已有文档要更新**

> **禁止对已有功能新开文档。** 如果新功能是对已有功能的补充/增强，必须**读已有文档**，然后在原文档上更新。

| 情况 | 操作 |
|------|------|
| 新功能属于全新子系统 | 新建 `docs/design/XXX.md`（用 DESIGN_TEMPLATE） |
| 新功能是已有功能的补充（如 restore 是 File Snapshot 的延伸）| **更新已有文档**（FILE_SNAPSHOT.md 加新章节），不新建 |
| 新功能是 RPC 对齐（如 tier_models）| 更新 `docs/design/PI_RPC_ALIGNMENT.md`，不新建 |
| 新功能是扩展能力（如 on_model_select &mut）| 更新 `docs/design/EXTENSION_SYSTEM.md` + AGENTS.md 已完成段 |

**第三步：选模板 + 写文档**

| 文档类型 | 模板 | 放哪 |
|---------|------|------|
| 内核功能设计 | DESIGN_TEMPLATE | `docs/design/` |
| CLI 验证用例 | CLI_TEST_TEMPLATE | `docs/testing/` 或附在设计文档里 |
| 验收规格（给评审方）| TEST_SPEC_TEMPLATE | `docs/testing/` |
| WASM 扩展手册 | EXTENSION_MANUAL_TEMPLATE | `{extension}/MANUAL.md` |
| pi 对齐调研 | PI_ALIGNMENT_TEMPLATE | `docs/design/` |

**第四步：写 CLI 验证（Group A/B/C 格式 + 完整命令 + 响应 JSON）**

每个功能**必须有 CLI 验证**，参照 BASH_EXTENSION.md / COMPACTION.md 的 Group 格式：

文档中**每个 RPC 必须给出**：
1. **完整的 `ion rpc` / `ion` 命令**（不能只写"调用 xxx 方法"）
2. **请求参数表**（字段/类型/默认/说明）
3. **完整响应 JSON**（成功 + 失败两种）
4. **验证点清单**（✅ 标记）

**格式示例**（参照 [CLI_TEST_TEMPLATE](./docs/templates/CLI_TEST_TEMPLATE.md)）：

```markdown
### RPC 接口规格

**请求：**
```bash
ion rpc --session <sid> --method get_flags \
  --params '{"extension":"my-ext"}'
```

**请求参数：**
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `extension` | string | 必填 | 扩展名 |

**响应 JSON（成功）：**
```json
{"success":true,"data":{"verbose":false,"max_items":100}}
```

**响应 JSON（失败）：**
```json
{"success":false,"error":"extension 'my-ext' not found"}
```

### Group A: 基础功能

#### A1 查询 flag
```bash
ion rpc --session sess_xxx --method get_flags \
  --params '{"extension":"my-ext"}'
```
**验证点：**
- ✅ 返回所有 flag 的当前值
- ✅ 包含 default 值
```

同时放在：
- 设计文档的"CLI 测试指南"章节（如 BASH_EXTENSION.md §0.2 的格式）
- `tests/xxx_ci.sh`（自动化脚本，可一键验证）
- 验证脚本登记到 AGENTS.md 测试统计表

**第五步：更新 AGENTS.md**

功能做完后，**必须更新 AGENTS.md**：
- 已完成段加描述 + 验证方法
- 测试统计表加新测试的数量
- 源码导航加新模块（如果有）
- 路线图标 ✅

### 扩展手册规范

每个扩展**必须**在其源码目录下维护一份 `MANUAL.md`，格式参照 [EXTENSION_MANUAL_TEMPLATE.md](./docs/templates/EXTENSION_MANUAL_TEMPLATE.md)。

| 要求 | 说明 |
|------|------|
| 文件 | `{extension}/MANUAL.md`，与 Cargo.toml 同级 |
| 格式 | 参照模板，覆盖工具/存储/事件/测试四节 |
| 构建 | `cargo build --target wasm32-wasip1 --release` |
| 安装 | `.wasm` 放入 `<project>/.ion/extensions/` 自动发现 |
| 集合 | 用户可通过 `ion extension list --docs` 浏览所有已安装扩展的手册 |

现有扩展手册：
- [todo-extension/MANUAL.md](./todo-extension/MANUAL.md) — 待办任务管理 (WASM)
- MEMORY 扩展手册（内核内置，见 [docs/design/MEMORY_EXTENSION.md](./docs/design/MEMORY_EXTENSION.md)）

### 例外

以下内容可以直接写在 AGENTS.md 中：
- **路线图**（`P0` / `P1` / 等）——仅列标题和状态，细节外链
- **架构图**——简短的 ASCII 架构描述
- **命令速查**——`cargo build / test / run` 等
- **文件路径结构**——`~/.ion/` 目录树

## 快速导航

### 设计文档（docs/design/）

| 文档 | 内容 |
|------|------|
| [docs/design/EXTENSION_SYSTEM.md](./docs/design/EXTENSION_SYSTEM.md) | WASM 扩展系统：热更新、4 维数据存储、16 个宿主函数 (已完成) |
| [docs/design/BASH_EXTENSION.md](./docs/design/BASH_EXTENSION.md) | Bash 扩展：同步执行 + 后台进程 + 综合教程 + CLI 测试 (设计稿+已实现) |
| [docs/design/MEMORY_EXTENSION.md](./docs/design/MEMORY_EXTENSION.md) | Memory 扩展 v0.1：大纲索引、异步检索、XML 注入、4 维存储 (已验证，搜索 bug 已修) |
| [docs/design/MEMORY_AGENT.md](./docs/design/MEMORY_AGENT.md) | Memory V0.2 跨项目记忆 Agent：单例扩展 + SQLite/FTS5 + 引用计数 (Phase 1-8 已实现) |
| [docs/design/CRASH_RECOVERY.md](./docs/design/CRASH_RECOVERY.md) | Worker 崩溃恢复：stderr 捕获 + exit code + Dead 保留 + 父通知 (已实现) |
| [docs/design/COMPACTION.md](./docs/design/COMPACTION.md) | Compaction 会话压缩：分批并发 + LLM summarizer + emergency fallback + CLI 测试 (已验证) |
| [docs/design/CONTEXT_INDEX.md](./docs/design/CONTEXT_INDEX.md) | Context Index — 上下文索引与快照折叠：read 追踪 + 过期快照折叠 + pi 对标 (V1 已实现，V2 待定) |
| [docs/design/PROVIDER_PROTOCOL.md](./docs/design/PROVIDER_PROTOCOL.md) | 多 Provider 协议：4 个 provider + transform_messages + detectCompat + CLI 测试 (已验证) |
| [docs/design/PERMISSION_SYSTEM.md](./docs/design/PERMISSION_SYSTEM.md) | 权限系统：设计 + CLI 用法 + 测试规格 + CLI 测试指南 (设计稿+已验证) |
| [docs/design/SESSION_MESSAGE.md](./docs/design/SESSION_MESSAGE.md) | Session 消息系统：Entry 类型、推送通道、消息类型扩展 (设计稿+已验证) |
| [docs/design/APPLE_CONTAINER_EXTENSION.md](./docs/design/APPLE_CONTAINER_EXTENSION.md) | Apple Container Backend：Group A-J 26 条测试用例 (已验证) |
| [BACKEND_TYPES.md](./BACKEND_TYPES.md) | Backend 类型分类：Local/Sandbox/Remote/Container + 5 种配置场景 (已完成) |
| [ROUTER_TEST_SPEC.md](./ROUTER_TEST_SPEC.md) | 路由层测试规格：68 条用例覆盖路由/路径/安全/配置错误 (已完成) |
| [docs/design/EXTENSION_ECOSYSTEM.md](./docs/design/EXTENSION_ECOSYSTEM.md) | Extension 生态验证：子 Worker 创建 + 事件发射 + CLI 验证 (已验证) |
| [docs/design/HOOK_SYSTEM.md](./docs/design/HOOK_SYSTEM.md) | Shell Hook 系统设计 (TRAE 兼容, 已被 HOOKS_AND_OUTLINE_SYNC 取代) |
| [docs/design/HOOKS_GUIDE.md](./docs/design/HOOKS_GUIDE.md) | **Hooks 使用指南**（内容文档，0 代码）：是什么/怎么配/CLI 怎么调/数据链路/大纲同步用例/FAQ (开发中) |
| [docs/design/HOOKS_AND_OUTLINE_SYNC.md](./docs/design/HOOKS_AND_OUTLINE_SYNC.md) | **Hooks 实现规格**（给写代码的人）：Rust 数据结构 + handler 执行引擎 + 补丁 1/2 改动清单 + bug fix (补丁 1 ✅ / 补丁 2 ✅) |
| [docs/testing/HOOKS_CLI_TEST.md](./docs/testing/HOOKS_CLI_TEST.md) | **Hooks CLI 测试指南**：RPC 接口规格 + Group A-H 验证用例 + 完整请求/响应 JSON (Group A ✅) |
| [docs/design/PERMISSION_STORE.md](./docs/design/PERMISSION_STORE.md) | Stored-Decision 权限记忆：用户选"always allow"后持久化，下次自动放行 (已完成) |
| [docs/design/SKILL_TOOL.md](./docs/design/SKILL_TOOL.md) | Skill 工具：让 LLM 按需调用 skill（不是启动时注入）+ list/inject/fork 模式 (待定) |
| [docs/design/PROVIDER_PROTOCOLS_TODO.md](./docs/design/PROVIDER_PROTOCOLS_TODO.md) | 缺失 Provider 协议规划：Mistral/Azure/Codex/Vertex/Bedrock 5 个协议补齐方案 (待定) |
| [docs/design/EXTENSION_HOST_API.md](./docs/design/EXTENSION_HOST_API.md) | Extension Host API：ctx.fs 统一文件访问 + WASM 文件读取 + 4 级数据目录 (待定) |
| [docs/design/TEAM_ORCHESTRATION.md](./docs/design/TEAM_ORCHESTRATION.md) | Team 编排（agent.md 驱动）— `ion --host --agent coordinator` 拆任务开发 (已验证) |
| [docs/design/WORKFLOW_GATE.md](./docs/design/WORKFLOW_GATE.md) | Workflow Gate — 内核级交付校验 (已完成) |
| [docs/design/WORKFLOW_ENGINE.md](./docs/design/WORKFLOW_ENGINE.md) | Workflow Engine — 结构化交付流水线 DSL + 执行流程 + CI Group (已验证) |
| [docs/design/PI_RPC_ALIGNMENT.md](./docs/design/PI_RPC_ALIGNMENT.md) | pi RPC CLI 对齐文档 (66 ✅ / 0 ❌ 全部对齐) |
| [docs/design/CLI_ARCHITECTURE.md](./docs/design/CLI_ARCHITECTURE.md) | CLI 三种执行场景设计：三场景分组验证用例 (设计稿，已被 CLI_PLAN 合并) |
| [docs/design/CLI_ROADMAP.md](./docs/design/CLI_ROADMAP.md) | CLI 落地路线图 (排期中，已被 CLI_PLAN 合并) |
| [docs/design/CLI_PLAN.md](./docs/design/CLI_PLAN.md) | **CLI 完整落地方案（唯一入口）**：架构 + 路线图 + 验证用例 + checklist 合并 (已完成) |
| [docs/design/FAUX_PROVIDER.md](./docs/design/FAUX_PROVIDER.md) | FauxProvider 架构级 LLM Mock：FIFO 队列 + 工厂响应 + 流式分块，对标 pi (已实现 Phase 1) |
| [docs/design/RECORD_REPLAY.md](./docs/design/RECORD_REPLAY.md) | Record/Replay 录制回放：环境变量录制 + `--model replay/id` 回放，复用 FauxProvider (已实现 Phase 1) |
| [docs/design/SESSION_TREE.md](./docs/design/SESSION_TREE.md) | Session Tree（会话分支）：文件内分支 + leaf 指针 + only-append 回滚 (已实现) |
| [docs/design/MCP_SYSTEM.md](./docs/design/MCP_SYSTEM.md) | MCP 系统：rmcp 1.x + 方案 C 共享池 + 权限控制 + resources/prompts + 热更新 (Phase 1-4 全部实现) |
| [docs/design/CONFIG_DIMENSIONS.md](./docs/design/CONFIG_DIMENSIONS.md) | 配置与数据维度分析：5 类存储划分 + 组件归属全表 + worktree 副本预期 + StorageContext 统一抽象 + 新扩展开发指南 (已实现) |
| [docs/design/FILE_SNAPSHOT.md](./docs/design/FILE_SNAPSHOT.md) | File Snapshot：双路快照（工具级 before/after + 目录扫描 + turn_end 兜底），restore_files + --restore-code 联动回滚，不遵守 .gitignore (已实现 + 2026-07-11 修复 5 个正确性问题) |
| [docs/design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md](./docs/design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md) | File Snapshot & Review 对齐清单：ION vs pi 全维度对比 + tree 快照模型升级路线 + per-file 审批 + 4 步执行计划 (开发中) |
| [docs/design/MESSAGE_RETRIEVAL_DESIGN.md](./docs/design/MESSAGE_RETRIEVAL_DESIGN.md) | 消息拉取 UI 设计规格：TypeScript 接口定义 + 6 种 UI 风格 + 3 层数据架构 (设计定稿) |
| [docs/design/SOFT_DELETE_COMPACT.md](./docs/design/SOFT_DELETE_COMPACT.md) | 软删除/软压缩内核机制：mark_deleted/summarized/restore + on_context 时序 (已实现) |
| [docs/testing/MESSAGE_RETRIEVAL_CASES.md](./docs/testing/MESSAGE_RETRIEVAL_CASES.md) | 消息拉取 CLI 用例集：9 接口 + 12 Group A-L + 分页/视点/过滤/血缘 (设计定稿+已实现) |
| [docs/design/MEMORY_ACTIVE.md](./docs/design/MEMORY_ACTIVE.md) | Memory Active — V0.2 主动注入（on_input→on_context 自动检索全局库）+ 自动整理（去重/归档/大纲索引）(待定) |

### 使用指南（docs/guides/）

| 文档 | 内容 |
|------|------|
| [docs/guides/CLI_USAGE.md](./docs/guides/CLI_USAGE.md) | CLI 标准用法：RPC / Subscribe / Extension RPC / Tool RPC 完整速查 (已验证) |
| [docs/guides/DEPLOY_ARCH.md](./docs/guides/DEPLOY_ARCH.md) | 部署架构 — 场景 + CLI 验证 |
| [docs/guides/EXTENSION_WORKFLOW.md](./docs/guides/EXTENSION_WORKFLOW.md) | 扩展开发测试工作流：写→build→安装→RPC 直调→LLM 引导→RPC 佐证 (已验证) |

### 测试（docs/testing/）

| 文档 | 内容 |
|------|------|
| [docs/testing/TEST_CASES.md](./docs/testing/TEST_CASES.md) | 完整测试 case (25 单元 + 32 集成 + 5 E2E + 5 压力) |
| [docs/testing/E2E_TEST_SPEC.md](./docs/testing/E2E_TEST_SPEC.md) | **全功能 E2E 测试规格**：12 Group 133 case，覆盖全部功能模块（基础执行/会话/树/RPC/工具/MCP/Team/Memory/Snapshot/权限/Compaction/Workflow） |
| [docs/testing/SESSION_TREE_SPEC.md](./docs/testing/SESSION_TREE_SPEC.md) | Session Tree 验收规格：harness（基于 FauxProvider）+ P0/P1/XFail 分级 |
| [docs/testing/FILE_SNAPSHOT_CASES.md](./docs/testing/FILE_SNAPSHOT_CASES.md) | File Snapshot 审批与回滚 CLI 用例集：5 Group 27 case（Group R 回滚 / V 审批 / L 联动 / E 事件 / X 边界）+ 9 接口完整请求/响应 JSON (实测态，2026-07-13) |

### 模板（docs/templates/）

| 模板 | 触发时机 |
|------|---------|
| [docs/templates/DESIGN_TEMPLATE.md](./docs/templates/DESIGN_TEMPLATE.md) | 写新功能设计文档时 |
| [docs/templates/CLI_TEST_TEMPLATE.md](./docs/templates/CLI_TEST_TEMPLATE.md) | 写 CLI 测试指南（Group A/B/C/D）时 |
| [docs/templates/TEST_SPEC_TEMPLATE.md](./docs/templates/TEST_SPEC_TEMPLATE.md) | 写测试规格（P0/P1/XFail）给评审方时 |
| [docs/templates/PI_ALIGNMENT_TEMPLATE.md](./docs/templates/PI_ALIGNMENT_TEMPLATE.md) | 调研 pi 能力并规划对齐时 |
| [docs/templates/EXTENSION_MANUAL_TEMPLATE.md](./docs/templates/EXTENSION_MANUAL_TEMPLATE.md) | 写 WASM 扩展手册时 |

### 归档（docs/archive/）

被合并或被替代的旧文档，仅供历史查阅。详见 [docs/README.md §归档说明](./docs/README.md)。

| 文档 | 被谁替代 |
|------|---------|
| [FILESYSTEM_SNAPSHOT.md](./docs/archive/FILESYSTEM_SNAPSHOT.md) | [FILE_SNAPSHOT.md](./docs/design/FILE_SNAPSHOT.md)（双路快照重写） |

### 源码导航

| 文件 | 内容 |
|------|------|
| `src/bin/ion.rs` | 主 CLI (45+ 参数) |
| `src/bin/ion_worker.rs` | Worker 子进程 (75 RPC 命令) |
| `src/worker_registry.rs` | Manager 内存状态 + Worker 管理 |
| `src/worker_api.rs` | WorkerHandle + ExtensionApi (扩展 API) |
| `src/agent/` | Agent 循环 (内层+外层+扩展钩子) |
| `ion-provider/` | Provider 抽象独立 crate (OpenAI SSE + tool_calls) |
| `src/extension.rs` | WASM 扩展加载器（[详情](./docs/design/EXTENSION_SYSTEM.md)） |
| `stock-extension/` | WASM 扩展示例 |
| `examples/agents/` | Agent 模板（wf/orchestrator/coordinator/developer/merger/reviewer/publisher） |
| `examples/workflows/` | Workflow YAML 示例（delivery.wf.yaml） |
| `src/session_tree.rs` | Session Tree 核心数据层（leaf 指针/树构建/branch/rollback/checkout） |
| `src/storage_context.rs` | **StorageContext**：统一存储路径访问（5 维 + worktree 透明），所有扩展用它拿路径（[约定](./docs/design/CONFIG_DIMENSIONS.md#9-storagecontext)） |
| `src/file_snapshot/` | File Snapshot 双路快照（object_store/scanner/snapshot/diff/gc，[详情](./docs/design/FILE_SNAPSHOT.md)） |
| `src/mcp/` | MCP 客户端（McpManager + McpTool/McpProxyTool + rmcp 连接 + 自动重连 + resources/prompts，[详情](./docs/design/MCP_SYSTEM.md)） |
| `src/message_retrieval.rs` | 消息拉取核心逻辑（retrieve_messages/turns/inputs/turn_detail + view/过滤/分页） |
| `src/global_memory.rs` | 全局记忆库（SQLite + FTS5，跨项目检索） |
| `src/global_memory_ext.rs` | GlobalMemoryExtension（单例扩展，on_singleton_init + extension_rpc） |
| `src/hooks/`（规划中） | Hooks 系统：HooksConfig + HookExtension + 5 handler 执行引擎（command/http/prompt/agent/mcp_tool），[详情](./docs/design/HOOKS_AND_OUTLINE_SYNC.md) |

## 架构

### 三场景归属：两套引擎

场景 1 是**直接执行**（没有 host）。场景 2 和场景 3 共享同一套**host 引擎**（WorkerRegistry + 事件转发 + spawn_worker），区别只在对外暴露方式不同：

```
              ┌─ 场景 1：直接 spawn 子进程，不经过 host
              │   跑完即退，没有事件转发
              │
    同一套     ├─ 场景 2：临时 host + 事件泵 → stdout
    底层 API  │   递归 idle 自动关
    (spawn、   │
     await、  └─ 场景 3：常驻 host + Unix socket → 外部 UI
    channel)      不自动退，外部可全程接入
```

| 场景 | CLI | 引擎 | 事件出口 | 同步子任务 | 异步任务 | 退出方式 |
|------|-----|------|---------|-----------|---------|---------|
| **1. 快速执行** | `ion "做这个"` | 直接 spawn（无 host） | ❌ 无 | ✅ spawn→await | ❌ 进程退出子 Worker 被干掉 | 跑完即退 |
| **2. 快速编排** | `ion --host "做这个"` | host 引擎 | 事件泵 → stdout | ✅ | ✅ host 兜着 | 递归 idle 自动关 |
| **3. 常驻服务** | `ion serve` | host 引擎 + socket | socket → 外部 UI | ✅ | ✅ host 兜着 | 手动 shutdown |

> "manager" 是内部实现细节（管理 Worker 生命周期的组件），永远不出现在 CLI 中。用户不会看见或输入这个词。

`ion-team` 不存在——它的功能完全被 `ion --host --agent coordinator "做这个"` 覆盖（coordinator agent 通过 spawn_worker 工具自己拆任务，不需要任何硬编码编排逻辑）。

### 场景 1 流程图

```
终端                   进程内
┌──────┐   ┌──────────────────────────┐
│      │   │  cmd_run()               │
│ ion  │──→│  建工具集 + Agent        │
│      │   │  agent.run(message)      │
│      │   │    ├─ LLM 循环            │
│      │   │    ├─ 调 tool (read/write)│
│      │   │    ├─ spawn_worker(同步)  │
│      │   │    │    └─ spawn 子进程    │
│      │   │    │        await 等完    │
│      │   │    └─ 返回               │
│      │   └─ 进程退出                  │
└──────┘                              │
    ❌ 没有 host，不能异步              │
    ❌ 没有事件转发                     │
    ✅ 同步子任务能用                    │
```

### 场景 2 流程图

```
终端                              临时 host
┌──────┐  ┌──────────────────────────────────────────────┐
│      │  │  WorkerRegistry + 命令循环 + 事件泵           │
│ ion  │──│                                              │
│      │  │  spawn coordinator Worker (子进程)            │
│--host│  │    │                                          │
│      │  │    ├─ spawn_worker(dev, 同步)                 │
│      │  │    │    └─ host 创建子 Worker → await 完成   │
│      │  │    ├─ spawn_worker(dev, 异步)                 │
│      │  │    │    └─ host 创建子 Worker                 │
│      │  │    │       └─ 子 Worker 执行 → agent_end      │
│      │  │    └─ channel_send ← 子 Worker 过程通信      │
│      │  │                                              │
│      │  │  事件泵 → stdout (实时打印 text_delta)        │
│      │  │  ...全部 idle → 清理退出                      │
└──────┘  └──────────────────────────────────────────────┘

    ✅ 有 host，同步异步都行
    ✅ 事件泵 → stdout
    ❌ 没有 socket，外部工具接不了
```

### 场景 3 流程图

```
外部 UI / TUI / IDE 插件               常驻 host
┌─────────────────┐   ┌───────────────────────────────────────┐
│        socket    │   │  WorkerRegistry + 命令循环            │
│  Web UI          │   │  Unix socket → ~/.ion/host.sock      │
│  ┌───────────┐   │   │                                       │
│  │进度条     │   │   │  spawn Worker(子进程)                  │
│  │卡片       │◄──│───│  ├─ 同步：spawn → await （UI 可见）   │
│  │步骤状态   │   │   │  │  └─ 通过 socket 推 text_delta      │
│  │实时日志   │   │   │  ├─ 异步：spawn → agent_end（UI 可见）│
│  └───────────┘   │   │  │  └─ 通过 socket 推 agent_start    │
│                  │   │  │        → text_delta → agent_end    │
│  ion rpc 命令行  │   │  ├─ channel_send ← 过程通信          │
│  ┌───────────┐   │   │  ├─ subscribe → 事件流推给 socket    │
│  │create_   │───│───│  └─ 一直运行（不自动退）               │
│  │worker     │   │   │                                       │
│  └───────────┘   │   │                                       │
└─────────────────┘   └───────────────────────────────────────┘

    ✅ 有 host，同步异步都行
    ✅ 事件通过 socket 推给外部工具 ── UI 可渲染成卡片/进度条
    ❌ 不自动退出，需要手动 shutdown
```

### 同步子任务 vs 异步任务

```
同步子任务 (spawn + await)         异步任务 (spawn + agent_end)
───────────────────────────       ───────────────────────────
Agent: spawn_worker(dev,         Agent: spawn_worker(dev,
       "查文档")                       "监控日志")
Agent: await_worker(id)          Agent: 继续聊别的
       ────干活────                       ──子 Worker 发消息──
Agent: ← 拿结果                          channel_send 实时收
                                       ──子 Worker agent_end──
                                        host 检测到 → UI 更新
```

> `channel_send` 是**工作过程中**的通信（子 Worker 还在跑时跟 coordinator 交流进度、问问题），不是完成通知。完成通知通过 `agent_end` 事件检测。

### 退出条件（场景 2）

递归 idle 检测：

```
入口 Worker (coordinator) idle？
├─ 它 spawn 的子 Worker 1 idle？
│   └─ 子 Worker 的子 Worker idle？
├─ 子 Worker 2 idle？
└─ ...全部 idle
  → 没有后台进程在跑 → 清理退出
```

> 如果需要反复执行（loop），外面套一个 shell while 即可，底层该退出退出，该启动启动。

### 基础组件

```
ion "hello"              → 用户入口
ion --host "hello"       → 带 host 能力的入口
ion serve                → 常驻服务入口
ion-worker --mode rpc    → 内部 Worker 子进程 (JSONL over stdin/stdout)
```

### 通信协议: JSONL over stdin/stdout (对齐 pi)

```json
请求: {"id":"1","method":"prompt","params":{"text":"hello"}}
响应: {"id":"1","type":"response","command":"prompt","success":true,"data":{...}}
事件: {"type":"event","event":{"type":"text_delta","delta":"..."}}
```

### Worker 间通信

| 方式 | 说明 |
|------|------|
| `send_to_worker(id, msg)` | 点对点（知道对方 ID） |
| `send_to_session(sid, msg)` | 按会话 ID（自动启动如果没运行） |
| `channel_send(name, msg)` | 群聊广播（不需要知道对方 ID） |
| `subscribe(id)` | 订阅 Worker 事件流 |

## 当前进度

### ✅ 已完成

- CLI 45+ 参数 (对齐 pi 41 核心参数)
- Provider 抽象层 (`ion-provider` 独立 crate)
- Agent 循环 (内外两层 + 约 45 个 Extension trait 方法 + 23 已接入)
- 约 27 个内置工具 (read/write/edit/bash/grep/find/ls/calculator/echo + 7 Git + spawn/send/resume/await channel_send/kill + global_memory_search/save + branch_session + remote tool) + 真实 bash 执行
- 会话管理 (JSONL v3 + 实时索引 + fork/continue/resume + cwd-hash 分组)
- --export HTML (pi 模板)
- --agent (内置 build/explore/plan + 自定义 .md)
- --skill / --extension (JSON + WASM 扩展)
- config.json + auth.json 配置系统
- Manager 守护进程 (spawn Worker + IO Bridge + 事件转发)
- Worker 子进程 (约 124 个 RPC 命令 + 真实 LLM + 工具调用)
- WorkerHandle + ExtensionApi (扩展能 create_worker/send/channel_send/emit)
- WASM 扩展完整链路 (注册工具 + 内存读取 + WASM-backed 执行)
- Worktree 隔离 (创建/清理/分支保留, `reclaim()`, `ION_WORKTREE_ROOT` 生效)
- Manager command 管道 (Worker → Manager 命令回传, 子 Worker 创建)
- 重试机制 (`RetryConfig` + `retry_async` + `send_to_worker_retry` + Harness)
- 权限引擎 (`PermissionEngine` + `UiSystem` + Agent 集成)
- 命令守卫 (`CommandGuard`: 白名单 + 风险模式检测)
- `ion subscribe` — 实时事件流（Instance + Extension 两级）
- Extension RPC — 扩展私有方法调用（`extension_rpc`）
- `ExtensionApi::emit_extension_event()` — 扩展发射自定义事件
- `ExtensionEventBus` — 事件总线 + broadcast + backpressure
- `on_extension_rpc()` — AgentExtension 新增钩子
- 6 个 Worker 编排工具（spawn_worker / send_to_worker / resume_worker / await_worker / channel_send / kill_worker）
- 完整 steer/follow_up/abort/promote_follow_up 行为对齐 pi
- Unix socket IPC（Manager ↔ CLI client）
- `ion rpc` client — Manager 级 / Instance / Tool / Extension 四类 RPC
- `cli_usage.md` — 标准用法文档（见 [docs/guides/CLI_USAGE.md](./docs/guides/CLI_USAGE.md)）
- 路由层 (`BackendRegistry`: 命令前缀+路径前缀路由，替代 RouterRuntime)
- Apple Container 后端 (`AppleContainerRuntime`: 真隔离 Linux VM，同端口并行)
- 命令守卫三模式 (`CommandGuard`: whitelist/blacklist/open，50+ 风险模式)
- 跨平台安全框架 (`build_secured`: PermissionEngine + CommandGuard 配置驱动)
- WASM 数据 host functions 路径穿越检查 (`safe_join`: 防 ../../../ 逃逸)
- Sandbox profile 白名单化 (`deny default` + 白名单，替换旧黑名单)
- 配置驱动沙箱权限 (`CommandGuardConfig` 接入 config.json)
- `BACKEND_TYPES.md` — Backend 类型分类与安全防御层级文档
- `ROUTER_TEST_SPEC.md` — 路由层 68 条测试规格
- `tests/apple_container_ci.sh` — 26 条 Apple Container E2E 自动化测试
- **pi CLI 全面对齐** (Phase A-D，~30 个 flag/功能):
  - 别名/短名：`-p`/`--print`, `--system-prompt`, `--continue`/`-c`, `--resume`/`-r`, `--tools`/`-t`, `--output-schema`
  - 新功能：`--mode text|json|rpc`, `--max-turns` 默认无限, 管道 stdin 自动检测, `@file` 图片支持
  - Session：`--session-id`/`--session` 部分 UUID 匹配/路径参数, `--continue` 按 mtime 恢复, `--fork` 路径
  - Model：`--model provider/id:thinking` 三段式语法, `--models` 列表解析
  - 压缩：`--compact-model` 独立小模型压缩 (`with_compact_model`)
  - 工具：`--list-models` flag, `ion config list`, `ION_AGENT_DIR`/`ION_SESSION_DIR` 环境变量
- **Team 编排（agent.md 驱动，零内核策略）**:
  - 6 个 agent 模板（`examples/agents/`）：orchestrator / coordinator / developer / merger / reviewer / publisher
  - 3 种调度策略：串行 `wait=true` / 小批量并行 `wait=false` + `await_worker` / 后台同级 `peer`
  - worktree 隔离：`spawn_worker(worktree=true)` 让 developer 在独立分支干活
  - converge 闭环：developer 写代码 → merger 合并+cleanup → publisher 推送 GitHub
  - orchestrator 分阶段 pipeline：DEVELOP → MERGE → PUBLISH，gate 校验 + 自动重试
  - 反幻觉重试：LLM 没调工具时自动重试并注入 WARNING（`retry_on_no_tool_use`）
  - `disallowed_tools` 黑名单生效（之前被忽略的 bug 已修）
  - runtime 默认 local（不从全局继承），`--local`/`--remote` flag 即时切换
  - **验证**: 5 任务串行 converge + 3 阶段 pipeline（develop→merge→publish GitHub）全部通过
- **测试**: 488 个 Rust 测试 + 37 MCP CI + 30 hooks CI 全部通过 ✅（截至 2026-07-15）
- **消息拉取（Message Retrieval）** — 9 接口 + 分页/视点/过滤/turn 聚合（已验证）
  - `message_retrieval.rs` 纯函数模块（~1000 行）— retrieve_messages/turns/inputs/turn_detail
  - turn_summary entry — 每轮 turn 结束自动落盘（含 abort/error turn）
  - CompactionEntry 加 firstKeptEntryId + compaction 落盘
  - view 视点（live/since_compaction/branch/full）+ 可见性过滤层（deletion/segment_summary/回滚）
  - 游标分页（after/before/limit）+ complete_turn 补齐 + include_custom 三档
  - list_turns content 默认截断 200 字 + full_content 参数
  - get_tree 双模式（structure 骨架 / full 全部）
  - get_children 反向索引 + SessionMeta 血缘字段（parent_session/last_entry_id）
  - `ion history <sid>` CLI 命令
  - **验证**: 229 单元 + 24 CLI harness = 253 测试全过 ✅
- **会话列表（ion sessions）—— 按主仓库维度查询**（已验证）
  - `ion sessions` 默认过滤当前主仓库（自动聚合 worktree 会话）
  - `--all` 关闭过滤列全部项目；`--json` 脚本/UI 消费；`--limit N` 表格条数
  - 主仓库聚合：复用 `paths::project_key_git`（`--git-common-dir`），主仓库和 worktree 算出同一 key
  - 表格字段：ID/AGENT/MODEL/BRANCH/MSGS/TOKENS(IN/OUT/CA)/CREATED/UPDATED/WT(🌿)
  - JSON 含 tokenCacheRead/tokenCacheWrite + 全部 SessionMeta 字段
  - 旧实现（仅全量、无项目过滤、无 cache 列）已升级
  - 详见 [docs/guides/CLI_USAGE.md](./docs/guides/CLI_USAGE.md) §会话列表
- **File Snapshot（双路文件快照系统）**:
  - 路线 1：write/edit 工具级 before/after（100% 精确 diff，含 cwd 外文件）
  - 路线 2：bash 目录扫描兜底（mtime+size 快速过滤 + git ignore 智能过滤）
  - content-addressed object store（去重存储，100MB GC 封顶）
  - **zstd 压缩存储**（>64B 文件压缩，小文件明文，magic bytes 兼容旧数据；10MB 改动→实际存储 100KB-1MB）
  - project_key 用 git-common-dir（主仓库和 worktree 共享存储）
  - turnId 用随机 ID（`ts_xxxxxxxxxxxx`，48bit），不依赖下标，多次交替操作不覆盖
  - **快照按 session 隔离**（`session_id` 字段 + 路径 `tool/<session_id>/<turnId>.jsonl`，跨 session 不混淆）
  - 5 个 RPC：`get_modified_files` / `get_file_diff` / `get_batch_diffs` / `get_file_history` / `restore_files`
  - `restore_files` + `--restore-code`：消息+代码联动回滚（先恢复磁盘 → 再回滚消息 → 记录 restore_point）
  - **`--restore-code --restore-mode full`**：精确恢复完整磁盘状态（含删除 target 之后新增的文件，scan 截断时自动跳过删除防误删）
  - **`--restore-code` 穿越压缩点**：只恢复代码不回滚消息（快照层独立于压缩）+ 修复了 parentId=null 拦截失效 bug
  - GC：启动时分级清理（7天→1天→可达性分析）
  - **默认关闭**（config `"file-snapshot": {"enabled": true}` 开启）
  - **验证**: 56 单元 + 9 harness + 25 CI = 90 测试全过 ✅
- **Tier Models（模型分层别名）**:
  - `--model fast/pro/max` 别名解析（底层统一处理，混用具体模型）
  - `get_tier_models` / `set_tier_models` RPC（持久化到 config.json）
  - 兜底链：tier_models → DEFAULT_TIER_ALIASES → default_model
  - `on_model_select(&mut ctx)` 钩子改可变 — 扩展能覆盖模型选择（自定义策略）
  - **验证**: 9 CI 测试全过 ✅
- **Extension Flags（运行时扩展 flag 读写）**:
  - `get_flags` / `set_flag` RPC（ExtensionRegistry 运行时 flag 存储）
  - 支持所有 JSON 类型（bool/number/string/object/array）
  - 扩展内通过 `ExtensionRegistry::get_flag()` 读取
  - **验证**: 10 CI 测试全过 ✅

- **Stored-Decision 权限记忆（对齐 pi stored-decision.ts）**:
  - 用户选"always allow"后持久化决策，下次自动放行，不用反复确认
  - `DecisionSource` 枚举（Config vs Stored）+ `PermissionRule.source/created_at` 字段
  - `store_decision` / `list_stored` / `remove_stored` / `clear_stored` 4 个方法
  - `UiPermissionResult` 枚举（Allow/Deny/AlwaysAllowProject/AlwaysDenyProject）+ `store_from_ui_result` 便捷方法
  - 顶层 RPC（`permission_store_decision` 等）+ `extension_rpc` 双路径
  - source 隔离：clear/remove 只动 Stored 规则，Config 规则不受影响
  - serde rename_all 修复（Decision/Scope/DecisionSource 统一小写，磁盘格式一致）
  - 详见 [docs/design/PERMISSION_STORE.md](./docs/design/PERMISSION_STORE.md)
  - **验证**: 18 单元 + 23 CI 测试全过 ✅

- **Hooks 系统（配置式生命周期触发器，对齐 pi）**:
  - 5 模块 ~800 行：`src/hooks/{mod,handler_runner,matcher,stdin_builder,extension}.rs`
  - `hooks.json` 配置（全局 + 项目级合并，每次事件触发动态读 = 热重载）
  - 12 事件映射 → Extension trait（SessionStart/End/PreCompact/UserPromptSubmit/PreToolUse/PostToolUse/PostToolUseFailure/PermissionRequest/SubagentStart/SubagentStop/Stop/Notification）
  - 5 种 handler：command ✅（spawn bash + 退出码 0/2/3 协议）/ http ✅（POST + HTTPS 校验）/ prompt ✅（调 ApiRegistry 做 LLM 判断）/ agent ✅（Runtime::spawn_worker，真能调工具）/ mcp_tool 🔧 stub（方案 C 架构限制，用 command 替代）
  - Agent handler 递归保护：`ION_HOOK_DEPTH` 跨进程传递（入口 depth=0 能 spawn → 子 depth>=1 跳过），防 Stop 事件配 agent handler 死循环
  - `scripts/hooks_test.sh`（纯 bash 验证工具，不依赖 Rust）
  - 补丁 1：`ExtensionWorkerConfig` 字段补齐（agent/initial_prompt/worktree/allowed_tools/disallowed_tools/max_turns）
  - `Agent.runtime` 从 `Box<dyn>` 改 `Arc<dyn>`（让 HookExtension clone 共享）
  - **验证**: hooks_ci 8 + hooks_agent_ci 4 + hooks_e2e 10 + patch1 5 + hooks_agent_real 3（真实 LLM DeepSeek）= 30 测试全过 ✅

### 🔌 MCP 系统（Model Context Protocol，Phase 1-4 全部实现）

- **Phase 1（配置 + RPC）**：
  - `McpServerConfig`（Stdio/Http untagged enum）+ `IonConfig.mcp_servers`
  - 项目维度配置 `~/.ion/projects/<key>/config.json`（worktree 共享，不依赖 git）
  - 3 RPC：`get_mcp_servers` / `mcp_toggle_server` / `mcp_restart_server`
- **Phase 2（rmcp 真实连接 + 方案 C 共享池）**：
  - rmcp 1.x 接入（`client` + `transport-child-process` + `transport-streamable-http-client-reqwest`）
  - `McpManager`（connect_all/connect_one/call_tool/toggle/restart/read_resource）
  - 方案 C：host 持有 MCP 连接，所有 Worker 通过 bridge 代理（McpProxyTool）
  - `McpTool: Tool`（场景 1 直连版）+ `McpProxyTool`（场景 2/3 bridge 代理版）
  - 真实 E2E：`mcp-server-everything` 13 工具 + 7 resources + 4 prompts 发现
- **Phase 3（自动重连 + 事件推送）**：
  - lazy 重连（call_tool 失败时检测 is_closed + 连接错误重试）
  - 后台重连监控（指数退避：base 1s → max 30s，最多 3 次）
  - `mcp_connection_change` 事件推送
- **Phase 4（安全 + 协议覆盖 + 运维）**：
  - MCP 权限控制（permission rules 管 `mcp__*` 工具，通配符 `mcp__server__*`）
  - resources/prompts 发现 + `read_resource`（rmcp list_resources/list_prompts/read_resource）
  - 配置热更新（`mcp_reload` RPC，改 config.json 后不用重启 worker）
- **验证**: 28 MCP CI 全过 ✅（Group A-H: 配置/toggle/restart/错误/真实连接/共享池/场景1/权限）

### ⚙️ 配置维度缺口修复（5 项）

- **缺口 #1**：`merge_project` 深度合并（从 3 字段 → 全字段 HashMap/Option/Vec 分类合并）+ 9 单元测试
- **缺口 #2**：`project_root_for_config()` 统一回源（WASM/Agent/Skill/Permission/Memory 5 处 worktree 消费点）
- **缺口 #3**：`git_project_root()` + `project_key_git()` 抽取（file-snapshot project_key 委托统一入口）
- **缺口 #4**：`settings_path()` 路径统一（~/.ion/settings.json）
- **缺口 #5**：worktree 路径注释修正
- 详细分析：[docs/design/CONFIG_DIMENSIONS.md](./docs/design/CONFIG_DIMENSIONS.md)（5 类存储 + 组件归属全表 + 31 条验收用例）

### 🎭 FauxProvider（架构级 LLM Mock，对标 pi）

- `FauxProvider` — 注册成 `"faux"` ApiProvider，FIFO 队列回放预设响应
- 工厂函数响应 — `(context, options, state, model) -> AssistantMessage`，能根据 agent 发来的 context 动态返回
- 流式分块 — `faux_stream_blocks` 把响应切成 token 粒度的 TextDelta/ThinkingDelta/ToolCallDelta
- loud failure — 队列空时报错 `"No more faux responses queued"`，不静默通过
- Builder 函数 — `faux_text`/`faux_thinking`/`faux_tool_call`/`faux_assistant_message`
- `register_faux(&mut registry)` — 一行注册，返回 `Arc<FauxProvider>` 控制柄
- 免 API key、走完整 agent 链路、不污染真实 provider
- **测试**: 20 个测试全部通过 ✅（faux_test）

### 🌳 Session Tree（会话内分支）

- `LeafPointerEntry` — leaf 指针持久化（移动光标不删旧路径）
- `get_tree()` / `resolve_current_leaf()` — 按 parentId 建树 + 光标解析（对齐 pi）
- `--branch <id>` / `--branch-name <name>` — 从某条消息分叉
- `--checkout <name>` — 切换命名分支
- `--rollback <id>` / `--rollback-reason <text>` — 回滚（路径保留 + tombstone）
- `--fork-from-leaf <sid>/<entry-id>` — 从分支点提取新 session（记 parentSession）
- `ion session tree <sid>` / `ion session branches <sid>` — 树展示
- `branch_session` Agent 工具 — LLM 自主分叉
- only-append 不变量 — 所有操作只追加 entry，永不改/删旧行
- compaction 安全检查 — branch 穿越压缩点时拒绝
- **测试**: 28 单元 + 4 集成测试 ✅

### 🔄 Worker 崩溃恢复

- stderr 捕获 — `Stdio::piped()` 写到 `~/.ion/tmp/ion-worker-{id}.stderr`
- exit code 读取 — cleanup 路径调 `child.try_wait()`
- 崩溃识别 — `exit_code ≠ 0` → `WorkerStatus::Dead` + 保留 record
- exit_reason — stderr 最后 10 行 + 退出码
- 父通知 — `child_crashed` 事件推送到 event_subscribers + parent_event_tx
- `drain_until_agent_end` — 崩溃后立即返回错误（不干等 300s）
- GC — Dead record 超时自动清理
- **测试**: 6 E2E 用例 ✅

### 🎬 Record/Replay（LLM 决策录制回放）

- `ION_RECORD=<id>` 环境变量 — 录制真实 LLM 响应到 `~/.ion/recordings/<id>/trace.jsonl`
- `--model replay/<id>` — 回放（不联网，免 API key）
- `ion recordings` — 列出所有录制
- 复用 FauxProvider 作为回放引擎（`load_script` + FIFO 队列）
- 路径穿越防御 + 并发 lock + 文件权限（0600/0700）
- request_hash 记录（Phase 2 strict 校验铺路）
- **测试**: 11 单元 + 11 E2E ✅

### 🧠 Memory V0.2（跨项目记忆 Agent）

- **单例扩展机制** — `is_singleton()` + `singleton_key()` + 5 个生命周期钩子
  - `on_singleton_init` / `on_user_join` / `on_user_leave` / `on_last_user_gone` / `on_singleton_shutdown`
  - 引用计数由内核维护（Worker 崩溃不干掉单例）
  - 只在 `ion serve` 加载（场景 3，选择 A）
- `global-memory.db` — SQLite + FTS5 全文检索（跨项目）
- `GlobalMemoryExtension` — 单例扩展（`singleton_key = "global-memory"`）
- `global_memory_search` / `global_memory_save` — LLM 工具（用户 Worker 直接查全局库）
- V0.1 → V0.2 自动迁移（JSON → SQLite）
- extension_rpc 路由 — Manager 级直接调单例扩展
- **测试**: 6 单元 + 8 E2E ✅

### 🧠 Memory 扩展 v0.1

- `memory_save` — 主动保存记忆（LLM Tool + Extension RPC 双入口）
- `memory_search` — 主动搜索记忆（含 tag/category/description 匹配）
- `extension_rpc: save/list/search/forget/inspect` — CLI 调试入口
- `forget` — 软删除（`archived: true`），list/search 默认过滤
- `content_hash` — djb2 哈希，内容变化可靠检测
- `outline` 路径净化（只允许 `[a-zA-Z0-9_-]`）
- `on_system_prompt` — 自动注入 `<memory_outline>` XML 到 system prompt
- `on_input` — 关键词匹配 → hash 对比 → 标记待注入
- `on_context` — 发 LLM 前注入 `<memory_context>` XML 到 messages
- `injected.json` — 记录注入历史（outline/hash/turn），20 轮去重窗口
- `Consolidation` — 每 5 轮自动整理 index 计数
- `Transcript` — 每句话自动记录 `transcript/input.jsonl`
- `transcript_search` — 按关键词搜索历史输入
- 6 种事件：`memory_saved` / `memory_injected` / `memory_consolidated` / `memory_debug` / `memory_skipped` / `transcript_appended`
- `tests/memory_e2e.rs` — 6 个集成测试

### ✅ 已验证 (真实 LLM + 真实 API)

```
✅ RPC 75 命令全覆盖 (pi 格式对齐)
✅ Manager spawn Worker + IO Bridge (小助手 + 对讲机)
✅ 真实 LLM prompt (DeepSeek API / GLM-4.7)
✅ Worker 工具调用 (read Cargo.toml → tokio)
✅ 实时事件推送 (agent_start/text_delta/agent_end)
✅ 多 Worker LLM 并发 (A=hi B=hey 同时)
✅ Channel 广播 + 接收
✅ E1 代码审查流水线 (协调者 + 2 子 Worker 并行)
✅ E3 Channel 协作 (3 Worker)
✅ E4 会话恢复 (关闭→重启→记住 Alice)
✅ 10 Worker 压力测试
✅ 5 并发 worktree 隔离开发
✅ 50 轮连续对话无泄漏
✅ 20 次快速创建/销毁无僵尸
✅ ion serve + socket subscribe 实时事件流
✅ Memory: on_input → on_context → injected.json 注入链路
✅ Memory: 真实 LLM prompt 触发记忆召回并注入上下文
✅ Memory: call_tool memory_save/search 直调
✅ Memory: subscribe 实时收到 memory_saved 事件
✅ Subscribe: instance 级事件 (agent_start/text_delta/agent_end)
✅ extension_rpc: 扩展私有方法 CLI 直调
✅ transcript: 每句话自动记录到 input.jsonl
✅ WASM todo-extension: build → load → test 全流程
✅ MCP: mcp-server-everything 连接 + 13 工具发现 + echo/get-sum 调用成功
✅ MCP: 方案 C 共享池 — host 持有连接，Worker bridge 代理（进程只 1 份）
✅ MCP: 7 resources + 4 prompts 发现 + read_resource 读取成功
✅ MCP: permission rules Deny 拦截 mcp__* 工具（精确 + 通配符）
✅ MCP: mcp_reload 热更新（改 config 不重启）
✅ MCP: 场景 1（cmd_run）MCP 初始化 + 工具注册
✅ Memory: v0.1 统一到 V0.2 SQLite（memory_save 存的 global_memory_search 能搜到）
✅ 远程工具: register_remote_tool + unregister_remote_tool
```

### 🗺 路线图

**P0 - 近期（当前 sprint）:**
- ~~CommandGuard 白名单 + 风险检测~~ ✅ 已完成
- ~~权限引擎 Agent 集成~~ ✅ 已完成

**P1 - Runtime 抽象层（沙箱/远程/本地三模式切换）:** ✅ 已完成

四种 Runtime 实现 + 路由层全部完成，支持统一 `Runtime` trait + 配置驱动切换。

| Runtime | 类型 | 隔离 |
|---------|------|------|
| `LocalRuntime` | 直接执行 | 无 |
| `SandboxRuntime` | 权限过滤（sandbox-exec） | 弱（共享 fs） |
| `RemoteRuntime` | SSH 远程执行 | 强 |
| `AppleContainerRuntime` | 容器 VM 隔离 | 强 |
| `BackendRegistry` | 路由层（替代 RouterRuntime） | — |

详细文档见 [BACKEND_TYPES.md](./BACKEND_TYPES.md)、[APPLE_CONTAINER_EXTENSION.md](./docs/design/APPLE_CONTAINER_EXTENSION.md)。

**配置示例：**

```json
{
  "runtime": {
    "default": "shanbox",
    "backends": {
      "local":   {"type": "local"},
      "shanbox": {"type": "remote", "hostname": "shanbox"}
    },
    "routes": [
      {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
    ]
  }
}
```

**P2 - 路径权限配置（对齐 pi path-permissions.json）:** ✅ 两大类已完成

| 层级 | 类型 | 配置来源 | 作用范围 |
|------|------|---------|---------|
| **命令级** | CommandGuard | `config.json` 的 `command_guard` | 命令黑/白名单 + 风险模式 |
| **路径级** | PermissionRule | `~/.ion/settings.json` 的 `permissions.rules`（全局）+ `<project>/.ion/settings.json`（项目） | 文件读/写/删 + 命令路径 |

已完成：
- CommadGuard 三模式（whitelist/blacklist/open）+ 50+ 风险模式 ✅
- PermissionRule 热重载（CLI: `extension_rpc reload`，保留会话规则）✅ 已验证
- ~~风险模式可配置化~~（CmdGuard 三模式已在 config.json 中）✅

**P3 - UI 对接:** ✅ 已完成
- ~~HTTP/WS 对外接口~~（用户确认不做）
- ~~UiSystem 通知/确认/弹窗~~ ✅ CLI `subscribe --ui` + `ui_respond` 验证通过
- ~~审计日志~~ ✅ CommandGuard 决策持久化到 `~/.ion/agent/audit.jsonl`（JSONL 格式，CLI 验证通过）

**P4 - 扩展生态:** ✅ 已验证
- ~~扩展通过 ExtensionApi 创建子 Worker 的端到端验证~~ ✅ 已完成（含 2 个生产级 Bug 修复）
- ~~WASM 扩展在 Agent 钩子中全面可用~~ ✅ 已完成
- ~~扩展 emit 自定义事件 + 外部调用扩展 custom method~~ ✅ 已完成（事件发射 CLI 验证通过）
- 验证文档：[docs/design/EXTENSION_ECOSYSTEM.md](./docs/design/EXTENSION_ECOSYSTEM.md)

**P5 - 包管理（低优先级）:**
- install/remove/update 子命令

### 测试统计 (2026-07-15)

| 套件 | 数量 | 覆盖 |
|------|------|------|
| lib tests (核心逻辑) | 379 | Agent/Permission/Retry/CommandGuard/Session/SessionTree/GlobalMemory/Memory/Worker/MessageRetrieval/SessionJsonl/SessionIndex/ContextIndex/SoftDeleteCompact/FileSnapshot(object_store[+zstd压缩]/scanner/snapshot[+session_id]/diff/gc/restore[+XL3截断安全]/tree_store/approval)/TierModels/Hooks/StoredDecision |
| unit_rpc_test (RPC 协议) | 20 | U1-U20 RPC 命令覆盖 + 接口格式兼容 |
| manager_integration (集成) | 25 | Manager + Worker + 事件 + UI + 消息拉取 |
| session_tree_test (集成) | 4 | only-append 审计/branch 接 leaf/全操作序列 |
| context_index_e2e (集成) | 3 | read→write 折叠/on_context 时序 |
| file_snapshot_harness (集成) | 9 | H1-H9：FauxProvider 驱动 agent loop 验证快照采集/on_gate_check/approve/reject/approve_all/**reject_all**/**approvals过滤**/**reject已有文件restored**/**re-approval重置** |
| e2e_stress (E2E + 压力) | 18 | E1-E4 E2E + S1-S4 压力 + 各种边界 |
| plugin_tests (扩展) | 17 | JSON/WASM/Plan/Todo 扩展 |
| worktree_isolation | 6 | worktree 创建/隔离/清洗 |
| child_worker / concurrency | 4 | 子进程通信/并发池 |
| memory_e2e | 6 | Memory 扩展存储/搜索/注入/去重 |
| ion-provider 单元 | 70 | OpenAI/Anthropic/Google/FauxProvider/RecordReplay/transform_messages |
| **小计 Rust 测试** | **488** | 全部通过 ✅ |
| faux_scenarios_ci (CLI E2E) | 4 | 三场景 faux（直接执行/host/serve） |
| record_replay_ci (CLI E2E) | 11 | 录制/回放/路径穿越/冲突/OVERWRITE/权限 |
| crash_recovery_ci (CLI E2E) | 6 | stderr/exit_code/Dead/父通知 |
| global_memory_ci (CLI E2E) | 8 | 单例生命周期/save/search/跨项目/软删除 |
| session_tree_ci (CLI E2E) | [废弃] | ION_FAUX_REPLY 造会话不落盘，被 session_tree_verify.sh 替代 |
| message_retrieval_ci (CLI E2E) | 55 | 消息拉取主验证（脚本 Group A-N 对应文档 A-M 场景）：ion history/分页/视点/turn_summary/compaction/turn 完整性/中断态/统计聚合/旁路数据/customType 两维属性/性能缓存/O(n)/血缘 |
| session_tree_verify (CLI E2E) | 15 | 树展示 + branch/rollback 单元测试 + 分支视点(live/full/since_compaction) + only-append 红线 + SESSION_TREE_SPEC P0 验收映射 |
| realtime_stitch_ci (CLI E2E) | 10 | Group I：host + create_session + subscribe + prompt + 事件流(agent_start/text_delta/agent_end) + 历史补齐 |
| file_snapshot_ci (CLI E2E) | 33+5 | Group A-L：object_store 去重[+zstd]/scanner 目录扫描/diff 生成/GC/4 RPC 端到端/worktree 并行/restore 恢复[+XL3 full mode]/审批 harness+RPC[+J2-J6]/事件推送 subscribe[+K1-K5]/**真实 LLM 审批闭环[+L1-L5, ION_E2E=1]** |
| tier_models_ci (CLI E2E) | 9 | Group T：get/set_tier_models RPC + --model fast/pro 别名解析 + 兜底 |
| extension_flags_ci (CLI E2E) | 10 | Group F：get_flags/set_flag RPC + 类型支持 + 缺参数报错 |
| mcp_ci (CLI E2E) | 37 | Group A-J：MCP 配置 + toggle + restart + 错误 + 真实连接 + 方案 C 共享池 + 场景 1 + 权限控制 + resources/prompts + read_resource + mcp_reload 热更新 |
| soft_delete_ci (CLI E2E) | 7 | 软删除/软压缩：mark_deleted/summarized/restore |
| overflow_recovery_ci (CLI E2E) | 5 | 上下文溢出恢复 |
| workflow_ci (CLI E2E) | 15 | Workflow Engine W1-W7 |
| sessions_ci (CLI E2E) | 20 | Group A-D：ion sessions 主仓库过滤/--all/JSON 字段完整性(含cache)/worktree 聚合/表格格式/非git降级 |
| hooks_ci (CLI E2E) | 8 | Group A-D：hooks 配置加载/热重载/validate/list + B.1 拦截--no-verify(command) + B.2 注入约定(command) + B.3 Stop检查测试(command) |
| hooks_agent_ci (CLI E2E) | 4 | Group E：agent handler 真能 spawn 子 Worker（FauxProvider 驱动）+ 死循环防护(hook_depth) + 子 Worker 跑完 |
| hooks_agent_real (CLI E2E, ION_E2E=1) | 3 | 真实 LLM (DeepSeek) 验证 agent handler 子 Worker 真能用 read 工具读文件 + 死循环防护 |
| hooks_e2e (集成) | 10 | 内核引擎：HooksConfig加载/handler_count/热重载/command block/no-verify/正常放行/注入上下文/Stop block+放行/agent handler不panic |
| patch1_worker_config (集成) | 5 | ExtensionWorkerConfig 字段序列化/透传/默认值/边界值 |
| permission_store_ci (CLI E2E) | 23 | Group A：stored-decision store/list/remove/clear + source 隔离(Config vs Stored) + session/project scope + extension_rpc 等价路径 + 错误处理 |
| **测试覆盖合计** | **771** | 全部通过 ✅（Rust 506 + CLI E2E 283，含 hooks 30 case + 真实 LLM 3 case） |

**P5 - 扩展钩子补全:** ✅
- ~~on_context 接入~~ ✅ (Memory 扩展 on_context 注入)
- ~~on_input 接入~~ ✅ (Memory 扩展 on_input 检索)
- ~~on_extension_rpc 接入~~ ✅ (Memory 扩展 Extension RPC)
- ~~session_before_compact / session_compact 接入~~ ✅
- ~~thinking_level_select~~ ✅ (已在 run() 中触发)
- session_before_switch / session_before_fork - 钩子已定义（trait + ExtensionRegistry），触发点待接（需 Runtime trait 扩展）
- session_tree - ✅ (SessionTree 已实现)
- user_bash / project_trust / resources_discover / ui - 后续 (需交互式 UI)

**P6 - Shell Hook 系统 (TRAE 兼容) ✅ 已完成:**
- 详细设计文档见 [docs/design/HOOKS_GUIDE.md](./docs/design/HOOKS_GUIDE.md)（使用指南）+ [docs/design/HOOKS_AND_OUTLINE_SYNC.md](./docs/design/HOOKS_AND_OUTLINE_SYNC.md)（实现规格）+ [docs/testing/HOOKS_CLI_TEST.md](./docs/testing/HOOKS_CLI_TEST.md)（CLI 测试）
- 内核补 2 块能力：(1) `ExtensionWorkerConfig` 字段补齐（agent/initial_prompt/worktree/allowed_tools/max_turns/hook_depth）；(2) Hooks 系统（HooksConfig + HookExtension + 5 种 handler 执行引擎，~800 行）
- handler 完成度：command ✅ / http ✅ / prompt ✅ / agent ✅（真能调工具，修 pi 的坑）/ mcp_tool 🔧 stub（方案 C 架构限制）
- 对齐 pi 的 `extensions/pi-hooks/`（12 事件 + 5 handler），修 pi 的坑：agent handler 真传 tools（不退化成单轮 LLM）
- hook_depth 跨进程递归保护（防 agent handler 死循环）：入口 Worker depth=0 能 spawn → 子 Worker depth>=1 跳过
- 热重载（每次事件触发动态读 hooks.json，改完即生效）
- 大纲同步（MD ↔ outline.json）作为配置式用例（纯 hooks.json + shell 脚本，0 行内核扩展代码）
- **验证**: hooks_ci 8 + hooks_agent_ci 4 + hooks_e2e 10 + patch1 5 = 27 测试全过

**P6c - MCP 生产化:** ✅ 已完成
- MCP Phase 1-4 全部实现（配置 + rmcp 连接 + 方案 C 共享池 + 自动重连 + 权限控制 + resources/prompts + 热更新）
- 详见 [docs/design/MCP_SYSTEM.md](./docs/design/MCP_SYSTEM.md) + [docs/design/MCP_PLAN_C.md](./docs/design/MCP_PLAN_C.md)
- Memory v0.1 统一到 V0.2 SQLite（两套不再割裂）
- 37 个 MCP CI 测试全过（Group A-J）

**P6b - 其他（待定）:**
- ~~@图片文件支持 (ContentBlock::Image 完整实现)~~ ✅ 已完成 — 3 provider 全部支持图片(OpenAI image_url / Anthropic source / Google inline_data)
- --models 多模型 Ctrl+P 切换 (交互式)
- Memory 扩展 v0.2 (~~SQLite 存储~~ ✅ / ~~FTS 检索~~ ✅ / ~~v0.1 统一~~ ✅ memory_save/search 走 GlobalMemoryStore / Active Memory — 主动注入+自动整理，见 [docs/design/MEMORY_ACTIVE.md](./docs/design/MEMORY_ACTIVE.md) (待定))
- ~~真实代码审查 E2E (当前用算术题代替)~~ ✅ 已完成 — E1 代码审查流水线(coordinator→reviewer 子 worker + channel)

**P8 - Workflow Engine:** ✅ 已验证
- DSL: workflow.yaml 结构化 stage 定义（id/agent/task/gate/if/loop_back/cleanup/outputs）
- 条件分支: `if: stages.X.status == 'done'` / `context.xxx == true` / `always`
- 上下文传递: `context:` 全局段 + `{{context.xxx}}` 模板变量 + `outputs:` 写入
- 持久化: yaml 即定义又即状态，断点恢复 + Agent 自写 workflow
- CLI: `ion workflow validate/run/status`
- CI: workflow_ci.sh 15 个测试用例全部通过（W1-W7）
- 详细设计: [docs/design/WORKFLOW_ENGINE.md](./docs/design/WORKFLOW_ENGINE.md)
- 内核已就绪: GateDecision + on_gate_check + spawn_worker + worktree（不需要改内核）

**P7 - 多 Provider 协议测试待办:**

已实现 4 个 provider + transform_messages，单元测试 37 个全过，e2e 真实 API 测试 4 个全过（Anthropic z.ai/glm-4.6 + OpenAI OpenCODE/deepseek-v4-flash）。

待测试（需要对应 API key）：
- `openai-responses` 真实 API（GPT-5 / o1 / o3 系列）— 验证 reasoning + tool_call + ID 回放
- `google-generative-ai` 真实 API（Gemini 2.5 Pro / Flash）— 验证 thinking + thoughtSignature
- `transform_messages` 跨 provider 切换 e2e（同一会话先用 openai-completions，再切 anthropic-messages，验证 thinking block 降级 + tool call ID 规范化）
- `detectCompat` 各 thinkingFormat 真实 API 验证（deepseek/zai/qwen/openrouter/together/ant-ling）
- `anthropic-messages` Claude 真实 API（非 z.ai 代理）— 验证 thinking signature + redacted thinking

测试方式：
```bash
# 单 provider 烟测
ION_E2E_ANTHROPIC=1 ION_ANTHROPIC_API_KEY="sk-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

ION_E2E_OPENAI=1 ION_OPENAI_API_KEY="sk-xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture

# Google (待添加 ION_E2E_GOOGLE 配置)
ION_E2E_GOOGLE=1 ION_GOOGLE_API_KEY="xxx" \
cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
```

剩余 provider 暂不实现（按用户要求，常见够用即可）：
- `azure-openai-responses` — Azure 部署的 OpenAI Responses API
- `openai-codex-responses` — Codex 专用
- `google-vertex` — Vertex AI
- `mistral-conversations` — Mistral
- `bedrock-converse-stream` — AWS Bedrock
- `cloudflare-workers-ai` — Cloudflare

## 文件系统路径 (对齐 pi)

```
~/.ion/                           ← 根目录 (ION_WORKTREE_ROOT 可覆盖 worktree 位置)
├── config.json                   ← 用户设置 (default-provider/model/api-key/base-url)
├── auth.json                     ← API Key (权限 600)
├── agent/
│   ├── sessions/                 ← 会话文件 (JSONL v3)
│   │   └── {session_id}.jsonl
│   ├── sessions.index.json       ← 实时索引 (O(1) 统计, per-turn 更新)
│   ├── last_session              ← 最近会话 ID
│   ├── agents/                   ← 自定义 Agent .md
│   │   └── reviewer.md
│   ├── skills/                   ← 全局技能
│   ├── prompts/                  ← 全局提示模板
│   ├── extensions-data/          ← 扩展全局数据
│   │   └── {ext_name}/
│   ├── project-data/             ← 扩展项目级数据
│   │   └── {hash}--{name}/
│   └── cache/                    ← 缓存
├── global-memory.db              ← 全局记忆库 (SQLite + FTS5, 跨项目)
├── projects/                     ← 项目维度配置 (② 不依赖 git 同步, worktree 共享)
│   └── <project_key>/            ← project_key = git common dir 的 hash, 主仓库与 worktree 一致
│       └── config.json           ← 项目维度配置 (MCP server / 本地 tier models)
├── worktree/                     ← Git worktree 隔离
│   └── {session_id}/{project}/
├── recordings/                   ← Record/Replay 录制
│   └── {recording-id}/
│       ├── trace.jsonl           ← LLM 响应序列
│       ├── meta.json             ← 元信息
│       └── .lock                 ← 录制锁
└── tmp/                          ← 临时文件 (重启可回收)
    ├── ion-bash-{id}.log
    └── ion-tool-results/{slug}/

<project>/.ion/                   ← 项目级配置
├── settings.json                 ← 项目设置 (与全局深度合并)
├── agents/                       ← 项目级 Agent
├── skills/                       ← 项目级技能
└── path-permissions.json          ← 路径权限
```

关键路径说明:
- 会话按 session_id 平铺存储, 不像 pi 按 cwd hash 分组 (简化)
- worktree 路径: `~/.ion/worktree/{session_id}/{project_name}/`, 自动创建 git 分支 `ion-{session_id}`
- auth.json 权限 600, config.json 权限 644
- `ION_WORKTREE_ROOT` 环境变量可覆盖 worktree 物理存储根目录
- `ION_SESSION_DIR` 环境变量可覆盖会话目录
- `ION_API_KEY` 环境变量可覆盖 API key

### 「项目级」存储维度（速查）

> **详细分析与论证见 [docs/design/CONFIG_DIMENSIONS.md](./docs/design/CONFIG_DIMENSIONS.md)**（含组件归属全表、worktree 副本预期、5 个设计缺口）。

**核心规则**：git worktree 与其主仓库视为同一个项目。按"是否适合 git 追踪"分 5 类：

| 维度 | 存放目录 | 适合 git 追踪 | worktree 行为 | 典型内容 |
|------|---------|--------------|--------------|---------|
| **① 全局** | `~/.ion/config.json` | — | 天然全局 | provider/auth/全局 MCP |
| **② 项目维度** | `~/.ion/projects/<project_key>/` | ❌ | **共享**（同 key） | MCP server、本地 tier models（含本地路径/密钥） |
| **③ 仓库内** | `<project>/.ion/` | ✅ | 靠 git checkout | agent .md、skill .md、permissions rules |
| **④ Session** | `~/.ion/agent/sessions/<cwd_hash>/` | ❌ | 独立 | 会话历史、注入记录 |
| **⑤ 单例** | `~/.ion/agent/global-memory.db` | ❌ | 跨 worker 共享 | 全局记忆 DB、session 索引 |

**`<project_key>`**：复用 file-snapshot 的 git common dir hash 算法（`object_store.rs:213-232`），主仓库和所有 worktree 算出同一个 key。

**合并优先级**：环境变量 > ② 项目维度 > ③ 仓库内 > ① 全局 > 默认值

**⚠️ 已知缺口**（详见 CONFIG_DIMENSIONS.md §5）：
- `merge_project()` 只合并 3 个字段，`extensions`/`tier_models`/`runtime` 项目级写了无效
- `ION_PROJECT_ROOT` 只被 config.rs 消费，WASM/Agent/Skill/Permission 在 worktree 里读不到项目级资源
- project-data 用 cwd hash（worktree 独立），file-snapshot 用 git common dir hash（worktree 共享），两套不统一

## 开发命令

```bash
cargo build --bin ion              # 主 CLI
cargo build --bin ion-worker       # Worker 子进程
cargo build --bin manager-test     # Manager 测试程序
cargo test --lib                   # 61 个单元测试 (核心逻辑)
cargo test --test unit_rpc_test     # 20 个 RPC 协议测试 (U1-U20)
cargo test --test manager_integration # 9 个 Manager 集成测试 (I1-I8)
cargo test                          # 全部 111 个测试
cargo run --bin demo               # CLI demo
cargo run --bin agent-demo         # Agent Loop demo (真实 LLM)
```

### target 目录及时清理

Rust 编译产物（`target/`）体积增长很快，几次 `cargo build --release` 后轻松超过 10GB。**建议定期清理，不要积压。**

```bash
# 查看 target 大小
du -sh target/          # 通常 2-10GB

# 按需清理
cargo clean             # 全部删除（下次 build 全量编译）
cargo clean -p <crate>  # 只清理指定 crate
rm -rf target/debug/    # 只删 debug 产物（保留 release）

# 建议：每次大版本切换（更新 Rust toolchain / 切分支）后跑一次
cargo clean
```

## 环境配置

```bash
ion config set api-key "sk-xxx"    # 存到 ~/.ion/auth.json (权限 600)
ion config set default-model deepseek-v4-flash
ion "hello"                        # 直接运行
```

## 项目结构

```
ion/                          # 主项目
├── src/agent/                # Agent 循环 + 扩展 + 工具
├── src/worker_registry.rs    # Manager Worker 管理
├── src/worker_api.rs         # 扩展 API (WorkerHandle + ExtensionApi)
├── src/extension.rs             # WASM 扩展加载器
├── src/session_jsonl.rs      # JSONL v3 会话格式
├── src/session_index.rs      # 实时索引 (O(1) 统计)
├── src/bin/ion.rs            # 主 CLI
├── src/bin/ion_worker.rs     # Worker 子进程
├── stock-extension/             # WASM 扩展示例
├── AGENTS.md                 # 本文件
├── TEST_CASES.md             # 测试 case 文档
└── RPC_DIFF_REPORT.md        # RPC 对比报告

ion-provider/                 # 独立 Provider crate
├── src/types.rs              # Message/Model/Context/Usage/StreamEvent
├── src/provider/openai.rs    # OpenAI-compatible 实现 (SSE + tool_calls)
└── src/registry.rs           # ApiRegistry + ModelRegistry
```
