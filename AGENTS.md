# ION — AI Agent Orchestration Platform

> 一个用 Rust 实现的 AI Agent 编排平台，对齐 pi (pi-coding-agent) 的全部能力。

## ⚠️ 术语规范：统一使用 Extension，禁止使用 Plugin

**本项目所有可扩展能力统称为 Extension。禁止使用 "plugin"、"插件" 这两个词。**

### 两类 Extension（API 完全一致，29 个生命周期钩子）

| 类型 | 加载方式 | 可关闭 | 例子 |
|------|---------|--------|------|
| **内置 Extension** | Rust 编译进内核 | ✅ config.json `extensions.X.enabled = false` | Memory / Bash / Streaming |
| **运行时 Extension** | WASM 动态加载 (`.wasm`) | ✅ 不加载即可 | todo / stock / plan / 任何第三方 |

两者唯一的区别是"代码住哪"——编译进二进制 vs 运行时从文件加载。拿到的 `Extension` trait 接口、钩子、数据访问权限完全相同。

### WASM Extension ABI 符号约定

WASM 模块导出的 C 函数必须使用 `extension_` 前缀：
- `extension_version()` / `extension_init()` / `extension_execute_tool(...)`
- `extension_on_input(...)` / `extension_on_context(...)` / `extension_on_system_prompt(...)` 等 29 个钩子
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
| [docs/design/COMPACTION.md](./docs/design/COMPACTION.md) | Compaction 会话压缩：分批并发 + LLM summarizer + emergency fallback + CLI 测试 (已验证) |
| [docs/design/PROVIDER_PROTOCOL.md](./docs/design/PROVIDER_PROTOCOL.md) | 多 Provider 协议：4 个 provider + transform_messages + detectCompat + CLI 测试 (已验证) |
| [docs/design/PERMISSION_SYSTEM.md](./docs/design/PERMISSION_SYSTEM.md) | 权限系统：设计 + CLI 用法 + 测试规格 + CLI 测试指南 (设计稿+已验证) |
| [docs/design/SESSION_MESSAGE.md](./docs/design/SESSION_MESSAGE.md) | Session 消息系统：Entry 类型、推送通道、消息类型扩展 (设计稿+已验证) |
| [docs/design/APPLE_CONTAINER_EXTENSION.md](./docs/design/APPLE_CONTAINER_EXTENSION.md) | Apple Container Backend：Group A-J 26 条测试用例 (已验证) |
| [BACKEND_TYPES.md](./BACKEND_TYPES.md) | Backend 类型分类：Local/Sandbox/Remote/Container + 5 种配置场景 (已完成) |
| [ROUTER_TEST_SPEC.md](./ROUTER_TEST_SPEC.md) | 路由层测试规格：68 条用例覆盖路由/路径/安全/配置错误 (已完成) |
| [docs/design/EXTENSION_ECOSYSTEM.md](./docs/design/EXTENSION_ECOSYSTEM.md) | Extension 生态验证：子 Worker 创建 + 事件发射 + CLI 验证 (已验证) |
| [docs/design/HOOK_SYSTEM.md](./docs/design/HOOK_SYSTEM.md) | Shell Hook 系统设计 (TRAE 兼容, 暂不开发) |
| [docs/design/TEAM_ORCHESTRATION.md](./docs/design/TEAM_ORCHESTRATION.md) | Team 编排（agent.md 驱动）— `ion --host --agent coordinator` 拆任务开发 (已验证) |
| [docs/design/WORKFLOW_GATE.md](./docs/design/WORKFLOW_GATE.md) | Workflow Gate — 内核级交付校验 (已完成) |
| [docs/design/WORKFLOW_ENGINE.md](./docs/design/WORKFLOW_ENGINE.md) | Workflow Engine — 结构化交付流水线 DSL + 执行流程 + CI Group (已验证) |
| [docs/design/PI_RPC_ALIGNMENT.md](./docs/design/PI_RPC_ALIGNMENT.md) | pi RPC CLI 对齐文档 (开发中) |
| [docs/design/CLI_ARCHITECTURE.md](./docs/design/CLI_ARCHITECTURE.md) | CLI 三种执行场景设计：三场景分组验证用例 (设计稿，已被 CLI_PLAN 合并) |
| [docs/design/CLI_ROADMAP.md](./docs/design/CLI_ROADMAP.md) | CLI 落地路线图 (排期中，已被 CLI_PLAN 合并) |
| [docs/design/CLI_PLAN.md](./docs/design/CLI_PLAN.md) | **CLI 完整落地方案（唯一入口）**：架构 + 路线图 + 验证用例 + checklist 合并，~11h 6 Phase (待执行) |
| [docs/design/FAUX_PROVIDER.md](./docs/design/FAUX_PROVIDER.md) | FauxProvider 架构级 LLM Mock：FIFO 队列 + 工厂响应 + 流式分块，对标 pi (已实现 Phase 1) |
| [docs/design/RECORD_REPLAY.md](./docs/design/RECORD_REPLAY.md) | Record/Replay 录制回放：环境变量录制 + `--model replay/id` 回放，复用 FauxProvider (已实现 Phase 1) |
| [docs/design/SESSION_TREE.md](./docs/design/SESSION_TREE.md) | Session Tree（会话分支）：文件内分支 + leaf 指针 + only-append 回滚 (设计稿) |

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
| [docs/testing/SESSION_TREE_SPEC.md](./docs/testing/SESSION_TREE_SPEC.md) | Session Tree 验收规格：harness（基于 FauxProvider）+ P0/P1/XFail 分级 |

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
- Agent 循环 (内外两层 + 29 扩展钩子 + 21 已接入)
- 21 个内置工具 (read/write/edit/bash/grep/find/ls/calculator/echo + 7 Git + spawn/send/resume/await channel_send/kill) + 真实 bash 执行
- 会话管理 (JSONL v3 + 实时索引 + fork/continue/resume + cwd-hash 分组)
- --export HTML (pi 模板)
- --agent (内置 build/explore/plan + 自定义 .md)
- --skill / --extension (JSON + WASM 扩展)
- config.json + auth.json 配置系统
- Manager 守护进程 (spawn Worker + IO Bridge + 事件转发)
- Worker 子进程 (75 RPC 命令 + 真实 LLM + 工具调用)
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
- 21 个 Worker 编排工具（spawn_worker / send_to_worker / resume_worker / await_worker / channel_send / kill_worker）
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
- **测试**: 380 个测试全部通过 ✅

### 🎭 FauxProvider（架构级 LLM Mock，对标 pi）

- `FauxProvider` — 注册成 `"faux"` ApiProvider，FIFO 队列回放预设响应
- 工厂函数响应 — `(context, options, state, model) -> AssistantMessage`，能根据 agent 发来的 context 动态返回
- 流式分块 — `faux_stream_blocks` 把响应切成 token 粒度的 TextDelta/ThinkingDelta/ToolCallDelta
- loud failure — 队列空时报错 `"No more faux responses queued"`，不静默通过
- Builder 函数 — `faux_text`/`faux_thinking`/`faux_tool_call`/`faux_assistant_message`
- `register_faux(&mut registry)` — 一行注册，返回 `Arc<FauxProvider>` 控制柄
- 免 API key、走完整 agent 链路、不污染真实 provider
- **测试**: 20 个测试全部通过 ✅（faux_test）

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

### 测试统计 (2026-07-07)

| 套件 | 数量 | 覆盖 |
|------|------|------|
| lib tests (核心逻辑) | 90 | Agent/Permission/Retry/CommandGuard/Paths/Session/Worker/Memory |
| backend_registry (路由层) | 20 | BackendRegistry/AppleContainerRuntime/路径规范化/glob |
| command_guard (命令守卫) | 20 | whitelist/blacklist/open 三种模式 + 50+ 风险模式 |
| wasm_extension (安全) | 10 | 路径穿越检查/safe_join/规范化 |
| unit_rpc_test (Phase 1) | 20 | U1-U20 RPC 协议 + 会话存储 |
| plugin_tests (Phase 1) | 17 | U21-U25 JSON/WASM/Plan/Todo 扩展 |
| manager_integration (Phase 2-4) | 30 | I1-I32 Manager + Worker + 事件 + UI |
| e2e_stress (Phase 5-6) | 20 | E1-E4 E2E + S1-S4 压力 + RT/Perm/Bash |
| worktree_isolation | 6 | WT1-WT6 worktree 创建/隔离/清洗 |
| child_worker | 3 | 子进程 Worker 通信 |
| concurrency | 1 | 并发池 |
| memory_e2e | 6 | Memory 扩展存储/搜索/注入/去重/路径净化 |
| **小计 lib 单元测试** | **243** | 全部通过 ✅ |
| apple_container_ci (CLI E2E) | 26 | 容器生命周期/命令/文件/IP/路由/多容器并行 |
| p4_extension_ci (CLI E2E) | 9 | Extension 子 Worker 创建 + 通信 |
| p4_events_ci (CLI E2E) | 7 | Extension 事件发射 + EventBus |
| p2_hotreload_ci (CLI E2E) | 9 | PermissionExtension 热重载 |
| p3_audit_ci (CLI E2E) | 7 | 审计日志持久化 |
| p3_ui_ci (CLI E2E) | 6 | UI 系统 subscribe --ui + ui_respond |
| cli_alignment_ci (CLI E2E) | 28 | pi CLI 对齐：flag/别名/语法/模式 |
| compaction_ci (CLI E2E) | 10 | 会话压缩：持久化/触发/小模型 |
| scenario2_ci (CLI E2E) | 27 | 场景 2 (--host)：启停/编排/worktree/converge/session恢复 |
| team_e2e (CLI E2E) | 8 | Team 编排：coordinator→developer→reviewer |
| workflow_ci (CLI E2E) | 15 | Workflow Engine：DSL校验/单stage/条件分支/上下文/多stage/断点恢复 |
| **测试覆盖合计** | **395** | 全部通过 ✅ |

**P5 - 扩展钩子补全:** ✅
- ~~on_context 接入~~ ✅ (Memory 扩展 on_context 注入)
- ~~on_input 接入~~ ✅ (Memory 扩展 on_input 检索)
- ~~on_extension_rpc 接入~~ ✅ (Memory 扩展 Extension RPC)
- ~~session_before_compact / session_compact 接入~~ ✅
- ~~thinking_level_select~~ ✅ (已在 run() 中触发)
- session_before_switch / session_before_fork / session_tree - 后续 (需会话树功能)
- user_bash / project_trust / resources_discover / ui - 后续 (需交互式 UI)

**P6 - Shell Hook 系统 (TRAE 兼容) (暂不开发):**
- 详细设计文档见 [docs/design/HOOK_SYSTEM.md](./docs/design/HOOK_SYSTEM.md)

**P6b - 其他（待定）:**
- @图片文件支持 (ContentBlock::Image 完整实现)
- --models 多模型 Ctrl+P 切换 (交互式)
- Memory 扩展 v0.2 (SQLite 存储 / FTS 检索 / Active Memory sub-agent)
- 真实代码审查 E2E (当前用算术题代替)

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
├── worktree/                     ← Git worktree 隔离
│   └── {session_id}/{project}/
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
- `ION_WORKTREE_ROOT` 环境变量可覆盖 worktree 根目录
- `ION_SESSION_DIR` 环境变量可覆盖会话目录
- `ION_API_KEY` 环境变量可覆盖 API key

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
