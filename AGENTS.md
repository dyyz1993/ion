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
   - pi 没有的（如 worktree 隔离、多 Worker 团队）→ ION 原创设计，记录在 [TEAM_ARCH.md](./TEAM_ARCH.md)

## 文档规范

设计文档/功能文档**不要**直接展开在 AGENTS.md 中，而是按以下规范外链：

### 引用格式

```
| [文档名.md](./文档名.md) | 一句话描述 (状态) |
```

状态标注（括号标注在描述末尾）：
- **已完成** — 功能已实现并通过验证
- **已验证** — 功能已实现并经过真实场景测试
- **开发中** — 正在实现
- **暂不开发** — 已设计但未排期
- **待定** — 有想法但未形成设计

### 文档自身规范

每个外链文档应在开头标注自身状态，例如：

```markdown
# 文档标题

> **状态：暂不开发** — 本文档为设计规划，尚未实现。
```

### 扩展手册规范

每个扩展**必须**在其源码目录下维护一份 `MANUAL.md`，格式参照 [EXTENSION_MANUAL_TEMPLATE.md](./EXTENSION_MANUAL_TEMPLATE.md)。

| 要求 | 说明 |
|------|------|
| 文件 | `{extension}/MANUAL.md`，与 Cargo.toml 同级 |
| 格式 | 参照模板，覆盖工具/存储/事件/测试四节 |
| 构建 | `cargo build --target wasm32-wasip1 --release` |
| 安装 | `.wasm` 放入 `<project>/.ion/extensions/` 自动发现 |
| 集合 | 用户可通过 `ion extension list --docs` 浏览所有已安装扩展的手册 |

现有扩展手册：
- [todo-extension/MANUAL.md](./todo-extension/MANUAL.md) — 待办任务管理 (WASM)
- MEMORY 扩展手册（内核内置，见 [MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md)）

### 例外

以下内容可以直接写在 AGENTS.md 中：
- **路线图**（`P0` / `P1` / 等）——仅列标题和状态，细节外链
- **架构图**——简短的 ASCII 架构描述
- **命令速查**——`cargo build / test / run` 等
- **文件路径结构**——`~/.ion/` 目录树

## 快速导航

| 文件 | 内容 |
|------|------|
| [TEAM_ARCH.md](./TEAM_ARCH.md) | 单项目自治 Agent 团队架构 — `ion team` 命令设计 (开发中) |
| [TEST_CASES.md](./TEST_CASES.md) | 完整测试 case (25 单元 + 32 集成 + 5 E2E + 5 压力) |
| [RPC_DIFF_REPORT.md](./RPC_DIFF_REPORT.md) | ion-worker vs pi RPC 格式对比报告 |
| [HOOK_SYSTEM.md](./HOOK_SYSTEM.md) | Shell Hook 系统设计 (TRAE 兼容, 暂不开发) |
| [EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md) | WASM 扩展系统：热更新、4 维数据存储、16 个宿主函数 (已完成) |
| [EXTENSION_WORKFLOW.md](./EXTENSION_WORKFLOW.md) | 扩展开发测试工作流：写→build→安装→RPC 直调→LLM 引导→RPC 佐证 (已验证) |
| [CLI_USAGE.md](./CLI_USAGE.md) | CLI 标准用法：RPC / Subscribe / Extension RPC / Tool RPC 完整速查 (已验证) |
| [SECURITY_CLI_GUIDE.md](./SECURITY_CLI_GUIDE.md) | Security & Runtime CLI 测试指南：进程管理、权限拦截、异步审批流程 (设计稿) |
| [MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md) | Memory 记忆扩展设计：大纲索引、异步检索、XML 注入、4 维存储 (设计稿) |
| [MEMORY_SPEC.md](./MEMORY_SPEC.md) | Memory 扩展测试规格：P0/P1/XFail 分级、完整接口定义、验收标准 (已验证) |
| [BASH_EXTENSION.md](./BASH_EXTENSION.md) | Bash 进程管理扩展设计：后台进程、实时流、退出原因、CLI 测试 (设计稿) |
| [SESSION_MESSAGE.md](./SESSION_MESSAGE.md) | Session 消息系统：Entry 类型、推送通道、LLM/UI 消费决策树 (设计稿) |
| `src/bin/ion.rs` | 主 CLI (45+ 参数) |
| `src/bin/ion_worker.rs` | Worker 子进程 (75 RPC 命令) |
| `src/worker_registry.rs` | Manager 内存状态 + Worker 管理 |
| `src/worker_api.rs` | WorkerHandle + ExtensionApi (扩展 API) |
| `src/agent/` | Agent 循环 (内层+外层+扩展钩子) |
| `ion-provider/` | Provider 抽象独立 crate (OpenAI SSE + tool_calls) |
| `src/extension.rs` | WASM 扩展加载器（[详情](./EXTENSION_SYSTEM.md)） |
| `stock-extension/` | WASM 扩展示例 |

## 架构

```
ion "hello"              → 单实例 CLI (Agent + LLM)
ion manager start        → Manager 守护进程 (管理多个 Worker)
ion-worker --mode rpc    → Worker 子进程 (JSONL over stdin/stdout)
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
- `CLI_USAGE.md` — 标准用法文档
| [SECURITY_CLI_GUIDE.md](./SECURITY_CLI_GUIDE.md) | Security & Runtime CLI 测试指南：进程管理、权限拦截、异步审批流程 (设计稿) |

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
✅ ion manager + socket subscribe 实时事件流
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

**P1 - Runtime 抽象层（沙箱/远程/本地三模式切换）:**

设计目标：所有工具执行走统一 trait，换模式只需改一行配置。

```rust
/// ──────────────────────────────────────────────────────────────
/// Runtime trait — 所有工具执行的底层抽象
/// 切换模式只需替换 Agent 初始化时传入的 Runtime 实现
/// ──────────────────────────────────────────────────────────────

#[async_trait]
pub trait Runtime: Send + Sync {
    /// 执行命令（bash）
    async fn execute(&self, command: &str, timeout_secs: u64)
        -> Result<(String, String, i32), String>;

    /// 读文件
    async fn read_file(&self, path: &str) -> Result<String, String>;

    /// 写文件
    async fn write_file(&self, path: &str, content: &str) -> Result<(), String>;

    /// 编辑文件（sed 式替换）
    async fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<(), String>;

    /// 文件是否存在
    async fn path_exists(&self, path: &str) -> bool;

    /// 列出目录
    async fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;

    /// 删除文件
    async fn remove_file(&self, path: &str) -> Result<(), String>;

    /// Runtime 类型名（调试用）
    fn runtime_type(&self) -> &str;
}
```

三种预置实现（已规划，未实现）：

```rust
// ── 模式 1: 本地直接执行（当前行为，无沙箱）──
pub struct LocalRuntime;
// execute  → tokio::process::Command::new("sh")
// read     → tokio::fs::read_to_string
// write    → tokio::fs::write
// 权限检查 + 命令守卫通过中间件包装实现

// ── 模式 2: macOS sandbox-exec（沙箱隔离）──
pub struct MacOSSandboxRuntime {
    profile: SandboxProfile,  // 读写/只读/禁止目录
}
// execute  → sandbox-exec -f profile.sb sh -c "cmd"
// read     → sandbox-exec -f profile.sb cat path
// 自动生成 .sb 配置文件控制文件访问权限

// ── 模式 3: 远程执行（RPC 到另一台机器）──
pub struct RemoteRuntime {
    endpoint: String,   // "http://remote-host:8080/runtime"
    api_key: String,
}
// 所有操作通过 HTTP/RPC 转发到远程 Runtime 服务
// 远程 Runtime 服务可以运行在 Docker/VM 中
```

配置切换（`~/.ion/config.json`）：

```json
{
  "runtime": {
    "mode": "local"
    // 或 "sandbox"
    // 或 "remote"
  },
  "sandbox": {
    "profile": "default",  // 从 ~/.ion/sandbox/ 加载
    "whitelist": ["npm", "git", "cargo"],
    "writable_dirs": ["/tmp", "/var/folders"],
    "readonly_dirs": ["/usr", "/etc"],
    "blocked_dirs": ["~/.ssh", "~/.aws"]
  },
  "remote": {
    "endpoint": "https://runtime.example.com",
    "api_key": "sk-xxx"
  }
}
```

Agent 初始化时的切换逻辑：

```rust
let runtime: Box<dyn Runtime> = match config.runtime_mode {
    "local"   => Box::new(LocalRuntime::new()),
    "sandbox" => Box::new(MacOSSandboxRuntime::new(profile)),
    "remote"  => Box::new(RemoteRuntime::new(endpoint, api_key)),
    _         => Box::new(LocalRuntime::new()),  // 默认本地
};

// Runtime 自动包装中间件：
//   PermissionEngine.check() → CommandGuard.check() → Runtime.execute()
//   三者串联，切换模式不影响安全策略
let secured = SecuredRuntime::new(
    runtime,
    permission_engine,
    command_guard,
    audit_logger,
);

let agent = Agent::new(registry, model, system_prompt, tools, config)
    .with_runtime(secured);
```

工具链的改造量：
- Tool trait 的 `execute()` 改成 `execute(runtime: &dyn Runtime)`
- 所有工具内部调用 `runtime.read_file()` / `runtime.execute_command()` 等
- Agent 初始化时创建 `Runtime`，传入 `ToolRegistry`
- 约 15 个工具需要改，但每个改动很小（`std::fs` → `runtime.xxx`）

- `Runtime trait` 定义
- `LocalRuntime` 实现（现有行为封装）
- `SecuredRuntime` 中间件包装（权限+守卫+审计）
- Tool trait 签名改为接收 `&dyn Runtime`
- 15 个工具逐一迁移

**P1b - 沙箱实现（后续）:**
- macOS sandbox-exec profile 生成
- Docker 容器 Runtime
- Windows WSL2 兜底

**P2 - 规则配置化:**
- 规则从 `~/.ion/rules/` 目录加载 JSON/YAML
- 项目级规则 `<project>/.ion/rules/`
- PermissionRule 热加载（文件变化自动重载）
- 风险模式可配置化

**P3 - UI 对接:**
- HTTP/WS 对外接口 (subscribe_overview / subscribe_session / subscribe_channel)
- UiSystem 全面接入通知/确认弹窗
- 审计日志：谁在什么时候执行了什么命令

**P4 - 扩展生态:**
- 扩展通过 ExtensionApi 创建子 Worker 的端到端验证
- WASM 扩展在 Agent 钩子中全面可用
- 扩展 emit 自定义事件 + 外部调用扩展 custom method

**P5 - 稳定性:**
- 修复 i21/i22 偶发 LLM 超时测试
- 会话树导航 (navigate_tree)
- install/remove/update 包管理子命令

### 测试统计 (2026-07-05)

| 套件 | 数量 | 覆盖 |
|------|------|------|
| lib tests (核心逻辑) | 90 | Agent/Permission/Retry/CommandGuard/Paths/Session/Worker/Memory |
| unit_rpc_test (Phase 1) | 20 | U1-U20 RPC 协议 + 会话存储 |
| plugin_tests (Phase 1) | 17 | U21-U25 JSON/WASM/Plan/Todo 扩展 |
| manager_integration (Phase 2-4) | 30 | I1-I32 Manager + Worker + 事件 + UI |
| e2e_stress (Phase 5-6) | 20 | E1-E4 E2E + S1-S4 压力 + RT/Perm/Bash |
| worktree_isolation | 6 | WT1-WT6 worktree 创建/隔离/清洗 |
| child_worker | 3 | 子进程 Worker 通信 |
| concurrency | 1 | 并发池 |
| memory_e2e | 6 | Memory 扩展存储/搜索/注入/去重/路径净化 |
| **总计** | **193** | 全部通过 ✅ |

**P5 - 扩展钩子补全:** ✅
- ~~on_context 接入~~ ✅ (Memory 扩展 on_context 注入)
- ~~on_input 接入~~ ✅ (Memory 扩展 on_input 检索)
- ~~on_extension_rpc 接入~~ ✅ (Memory 扩展 Extension RPC)
- ~~session_before_compact / session_compact 接入~~ ✅
- ~~thinking_level_select~~ ✅ (已在 run() 中触发)
- session_before_switch / session_before_fork / session_tree - 后续 (需会话树功能)
- user_bash / project_trust / resources_discover / ui - 后续 (需交互式 UI)

**P6 - Shell Hook 系统 (TRAE 兼容) (暂不开发):**
- 详细设计文档见 [HOOK_SYSTEM.md](./HOOK_SYSTEM.md)

**P6b - 其他（待定）:**
- @图片文件支持 (ContentBlock::Image 完整实现)
- --models 多模型 Ctrl+P 切换 (交互式)
- Memory 扩展 v0.2 (SQLite 存储 / FTS 检索 / Active Memory sub-agent)
- 真实代码审查 E2E (当前用算术题代替)

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
└── rules/                        ← 规则文件
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
