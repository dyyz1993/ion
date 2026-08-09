# ION — AI Agent Orchestration Platform

> 一个用 Rust 实现的 AI Agent 编排平台，对齐 pi (pi-coding-agent) 的全部能力。

## ⚠️ 项目状态：未上线，无需兼容旧数据

**本项目当前处于开发阶段，尚未上线。** 所有数据格式变更**无需兼容旧数据**：

- session JSONL、SessionIndex、file-snapshot 等存储格式的 breaking change 可直接做，不需要写迁移逻辑
- 测试或开发产生的旧 session 文件可直接清理（`rm -rf ~/.ion/agent/sessions/`）
- 如果旧数据导致反序列化失败，直接删除重建即可，不需要 fallback 容错

## ⚠️ 术语规范：统一使用 Extension，禁止使用 Plugin

**本项目所有可扩展能力统称为 Extension。禁止使用 "plugin"、"插件" 这两个词。**

### 两类 Extension（API 完全一致，36 个生命周期钩子 + 27 个 host functions）

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

> **🔴 命令行可验证原则（硬性要求）**
>
> **每个功能都必须能用命令行（`ion rpc` / shell 脚本 / `ion subscribe`）从外部验证它真的工作——不只是 Rust 单元测试。** 这意味着：
>
> - 功能的行为必须**可观察**：要么有 RPC 能查结果（如 `extension_rpc`、`call_tool`），要么能通过 `subscribe` 看到事件（扩展在关键行为时 `emit` 事件）
> - 必须有配套的 `tests/<feature>_ci.sh` 脚本，起 host + 敲命令 + 断言结果
> - 如果一个功能只能用 Rust 测、命令行根本碰不到，说明它的**可观测性有缺陷**，要先补观察口（RPC/事件），再写 CI 脚本
>
> 参照 `tests/extension_fs_ci.sh`（ctx.fs 的命令行验证：起 host → `ion rpc extension_rpc fs_probe ...` → 断言）。

**Harness 优先原则**：先用 FauxProvider 写 harness 测试把闭环跑通（零 API 成本、确定性），验证通过后再补真实 case。

**FauxProvider 的两种模式**：
- **Static**（`ION_FAUX_REPLY` / `ION_FAUX_SCRIPT`）：固定响应序列，适合 CLI 冒烟、RPC 连通性测试
- **Factory**（Rust 闭包，`FauxResponseStep::Factory`）：根据 context 动态返回，适合审批/多轮交互等需要"根据上下文决定行为"的场景

**自查清单**（功能完成前必查）：
1. ✅ 有 harness 测试吗？（FauxProvider 驱动，不调真 LLM）
2. ✅ Factory 用在需要动态分支的场景了吗？（审批、多轮交互必须用 Factory）
3. ✅ 有 `#[ignore]` 真实 case 吗？（标 `ION_E2E=1` 触发）
4. ✅ 测试文档里有 harness 章节 + 真实 case 章节吗？
5. ✅ CLI 测试组按**用户场景**分 Group（不按技术维度）？核心链路全覆盖？（见 [CLI_TEST_TEMPLATE §测试组设计方法论](./docs/templates/CLI_TEST_TEMPLATE.md)）
6. ✅ 测试数据模拟真实场景？case 输入用用户自然语言？有性能/成本可测量指标？

> **🔴 CI 脚本进程清理规范（硬性要求）**
>
> 测试脚本清理 host 进程时**禁止用宽泛的 `pkill -f "ion"` 或 `pkill -f "ion.*serve"`**——系统里很多进程名含 "ion"（如 LogiOptionsPlus 罗技驱动），会被误杀。
>
> **系统里常见的 ion 相关进程（全部要排除，不能误杀）**：
> - `LogiOptionsPlus`（罗技鼠标/键盘驱动,名字含 "ion"）
> - `regiond` / `regions-daemon`(macOS 系统服务)
> - `notifications` / `UserNotif`(macOS 通知服务,含 "ion")
> - `Google Chrome Helper`（Chrome 渲染进程,命令行可能含 "ion"）
> - 任何 `/Applications/*.app/` 下的进程（都是用户应用,不是我们的 ion）
>
> 用 `ps aux | grep ion` 看到的进程,**绝大多数不是我们的 ion**——只有路径包含 `target/debug/ion` 或 `study-rust/ion` 的才是。
>
> 正确做法（按优先级）：
> 1. **精确 PID**（最安全）：脚本启动 host 时 `HOST_PID=$!`，cleanup 时 `kill "$HOST_PID"`
> 2. **按 socket**：`ion serve` 绑定 `~/.ion/host.sock`，可查占用者：`lsof -ti "$HOME/.ion/host.sock" | xargs kill`
> 3. **完整路径匹配**（如果必须 pkill）：`pkill -f "target/debug/ion serve"`（路径够具体）
>
> 禁止的模式：`pkill -f "ion"` / `pkill -f "ion serve"` / `pkill -f "ion.*serve"` / `pkill -f "ion_sse_proxy"`(单独写可以,但不能跟宽泛模式组合)。

**真实 LLM 测试推荐模型**：

| 用途 | 模型 | Provider | 说明 |
|------|------|----------|------|
| **B 改代码（主力）** | `glm-5.2` | `zai` | 代码质量好、UTF-8 稳定（无 U+FFFD 问题）、推理能力强 |
| **快速测试** | `deepseek-v4-flash` | `opencode` | 便宜快速，适合简单任务/CI |
| **Avoid** | claude-opus / gpt-4o | — | 昂贵，日常没必要 |

**模型配置**（`~/.ion/config.json`）：
```json
{
  "default_provider": "zai",
  "default_model": "glm-5.2",
  "tier_models": {
    "max": "zai/glm-5.2",
    "pro": "zai/glm-5.2",
    "fast": "opencode/deepseek-v4-flash"
  },
  "providers": {
    "zai": {
      "name": "zai",
      "api": "openai-completions",
      "base_url": "https://your-zai-proxy/v4",
      "api_key": "any-token-here",
      "models": [
        {"id": "glm-5.2", "name": "GLM-5.2", "reasoning": true, "context_window": 128000}
      ]
    }
  }
}
```

**A→B 自进化的模型选择**：
- `scripts/evolve_self.sh` / `evolve_batch.sh` 默认 `MODEL=glm-5.2 PROVIDER=zai`
- 快速任务可用 `MODEL=deepseek-v4-flash PROVIDER=opencode bash scripts/evolve_self.sh`

**实测对比**（同样的 B 任务）：

| 模型 | U+FFFD 数 | 代码质量 | 速度 |
|------|----------|---------|------|
| DeepSeek-V4-Flash | 经常引入 1-4 处 | 中等 | 快 |
| **GLM-5.2** | **0 处** ✅ | 高（自己理解 pattern） | 中等 |

> **结论**：GLM-5.2 处理 UTF-8 比 DeepSeek 稳定（不会破坏中文 comment），代码质量更好（自主选择正确的 API pattern）。A→B 自进化默认用 GLM-5.2。

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
- **plan 工具**（内核内置，非 WASM）— plan_enter/exit/add/list/done/approve，支持 strict_mode 强制用户审批
- MEMORY 扩展手册（内核内置，见 [docs/design/MEMORY_EXTENSION.md](./docs/design/MEMORY_EXTENSION.md)）

#### plan 工具（内核内置，commit 501697e/25b009e/10d4761）

**不要做成 WASM 扩展**——ION 内核已有内置 PlanExtension（`src/agent/plan_extension.rs`）+ PlanTool（`src/agent/plan_tool.rs`），提供 6 个工具：

| 工具 | 作用 |
|------|------|
| `plan_enter` | 进入计划模式（锁定 edit/write/bash，强制先规划） |
| `plan_exit` | 退出计划模式（持久化 PLAN.md，解锁工具） |
| `plan_add` | 加步骤 |
| `plan_list` | 列步骤（状态：`[ ]` pending / `[a]` approved / `[x]` done）|
| `plan_done` | 标记步骤完成 |
| `plan_approve` | 用户审批步骤（strict_mode 下必需）|

**strict_mode**（commit 10d4761）：`plan_enter(strict_mode=true)` 启用强制审批——`plan_exit` 要求所有步骤 approved 才放行，`plan_done` 要求步骤先 approved。默认 false（向后兼容，AI 自决）。

**踩过的坑**（别再踩）：
1. 工具名 `plan_enter`/`plan_exit` 不要做成 WASM——会跟内置 PlanExtension 的 plan mode 触发器冲突（之前 WASM 版 bug：进 plan mode 后 plan_add 被锁死）
2. PlanExtension 跟 PlanTool 必须共享同一个 `Arc<PlanExtension>` 实例（通过 `SharedPlanExtension` wrapper），否则 plan_add 写的状态 plan_exit 读不到（PLAN.md 空文件 bug）
3. strict 检查必须放在 `Tool::execute` 里，不能放 `after_tool_call`——因为 `agent.call_tool()`（RPC 路径）不调 after_tool_call

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
| [docs/design/ARCHITECTURE.md](./docs/design/ARCHITECTURE.md) | **总体架构总览**：5 层架构大图 + 三场景对比 + Worker 内部结构 + Agent 循环钩子时序 + 多智能体编排 + 5 维存储 + 6 条 ADR (已完成) |
| [docs/design/EXTENSION_SYSTEM.md](./docs/design/EXTENSION_SYSTEM.md) | WASM 扩展系统：热更新、4 维数据存储、27 个 host functions + 36 个生命周期钩子 + WASI stubs (已完成) |
| [docs/design/BASH_EXTENSION.md](./docs/design/BASH_EXTENSION.md) | Bash 扩展：同步执行 + 后台进程 + 综合教程 + CLI 测试 (设计稿+已实现) |
| [docs/design/MEMORY_EXTENSION.md](./docs/design/MEMORY_EXTENSION.md) | Memory 扩展 v0.1：大纲索引、异步检索、XML 注入、4 维存储 (已验证，搜索 bug 已修) |
| [docs/design/MEMORY_AGENT.md](./docs/design/MEMORY_AGENT.md) | Memory V0.2 跨项目记忆 Agent：单例扩展 + SQLite/FTS5 + 引用计数 (Phase 1-8 已实现) |
| [docs/design/LEARNING_EXTENSION.md](./docs/design/LEARNING_EXTENSION.md) | Learning Extension：session 结束后 LLM 提炼 skill + 密钥脱敏 + 幂等写入 (已完成) |
| [docs/design/SKILL_DISTILLATION.md](./docs/design/SKILL_DISTILLATION.md) | Skill Distillation：7 步流程（session 发现→消息提取→过滤→脱敏→LLM 提炼→文件写入）(已完成) |
| [docs/design/LSP_EXTENSION.md](./docs/design/LSP_EXTENSION.md) | LSP Extension：多语言诊断（Rust/TS/Python/Go/HTML）+ on_tool_execution_end 异步检查 + on_context 注入 (已完成) |
| [docs/design/SECRET_DETECTOR.md](./docs/design/SECRET_DETECTOR.md) | Secret Detector：密钥检测 + 脱敏（4.5 bits/char 熵检测）+ LearningExtension 集成 (已完成) |
| [docs/design/CRASH_RECOVERY.md](./docs/design/CRASH_RECOVERY.md) | Worker 崩溃恢复：stderr 捕获 + exit code + Dead 保留 + 父通知 (已实现) |
| [docs/design/COMPACTION.md](./docs/design/COMPACTION.md) | Compaction 会话压缩：分批并发 + LLM summarizer + emergency fallback + CLI 测试 (已验证) |
| [docs/design/CONTEXT_INDEX.md](./docs/design/CONTEXT_INDEX.md) | Context Index — 上下文索引与快照折叠：read 追踪 + 过期快照折叠 + pi 对标 (V1 已实现，V2 待定) |
| [docs/design/PROVIDER_PROTOCOL.md](./docs/design/PROVIDER_PROTOCOL.md) | 多 Provider 协议：4 个 provider + transform_messages + detectCompat + CLI 测试 (已验证) |
| [docs/design/PERMISSION_SYSTEM.md](./docs/design/PERMISSION_SYSTEM.md) | 权限系统：设计 + CLI 用法 + 测试规格 + CLI 测试指南 (设计稿+已验证) |
| [docs/design/SESSION_MESSAGE.md](./docs/design/SESSION_MESSAGE.md) | Session 消息系统：Entry 类型、推送通道、消息类型扩展 (设计稿+已验证) |
| [docs/design/APPLE_CONTAINER_EXTENSION.md](./docs/design/APPLE_CONTAINER_EXTENSION.md) | Apple Container Backend：Group A-J 26 条测试用例 (已验证) |
| [docs/design/BACKEND_TYPES.md](./docs/design/BACKEND_TYPES.md) | Backend 类型分类：Local/Sandbox/Remote/Container + 5 种配置场景 (已完成) |
| [docs/design/ROUTER_TEST_SPEC.md](./docs/design/ROUTER_TEST_SPEC.md) | 路由层测试规格：68 条用例覆盖路由/路径/安全/配置错误 (已完成) |
| [docs/design/EXTENSION_ECOSYSTEM.md](./docs/design/EXTENSION_ECOSYSTEM.md) | Extension 生态验证：子 Worker 创建 + 事件发射 + CLI 验证 (已验证) |
| [docs/design/HOOK_SYSTEM.md](./docs/design/HOOK_SYSTEM.md) | Shell Hook 系统设计 (TRAE 兼容, 已被 HOOKS_AND_OUTLINE_SYNC 取代) |
| [docs/design/HOOKS_GUIDE.md](./docs/design/HOOKS_GUIDE.md) | **Hooks 使用指南**（内容文档，0 代码）：是什么/怎么配/CLI 怎么调/数据链路/大纲同步用例/FAQ (开发中) |
| [docs/design/HOOKS_AND_OUTLINE_SYNC.md](./docs/design/HOOKS_AND_OUTLINE_SYNC.md) | **Hooks 实现规格**（给写代码的人）：Rust 数据结构 + handler 执行引擎 + 补丁 1/2 改动清单 + bug fix (补丁 1 ✅ / 补丁 2 ✅) |
| [docs/testing/HOOKS_CLI_TEST.md](./docs/testing/HOOKS_CLI_TEST.md) | **Hooks CLI 测试指南**：RPC 接口规格 + Group A-H 验证用例 + 完整请求/响应 JSON (Group A ✅) |
| [docs/design/PERMISSION_STORE.md](./docs/design/PERMISSION_STORE.md) | Stored-Decision 权限记忆：用户选"always allow"后持久化，下次自动放行 (已完成) |
| [docs/design/SKILL_TOOL.md](./docs/design/SKILL_TOOL.md) | Skill 工具：让 LLM 按需调用 skill（不是启动时注入）+ list/inject/fork 模式 (已完成) |
| [docs/design/PROVIDER_PROTOCOLS_TODO.md](./docs/design/PROVIDER_PROTOCOLS_TODO.md) | 全部 9 种 Provider 协议已实现（含 Azure/Codex/Vertex）(已完成) |
| [docs/design/EXTENSION_HOST_API.md](./docs/design/EXTENSION_HOST_API.md) | Extension Host API：ctx.fs 统一文件访问 + WASM 文件读取 + 4 级数据目录 (已完成) |
| [docs/design/TEAM_ORCHESTRATION.md](./docs/design/TEAM_ORCHESTRATION.md) | Team 编排（agent.md 驱动）— `ion --host --agent coordinator` 拆任务开发 (已验证) |
| [docs/design/WORKFLOW_GATE.md](./docs/design/WORKFLOW_GATE.md) | Workflow Gate — 内核级交付校验 (已完成) |
| [docs/design/WORKFLOW_ENGINE.md](./docs/design/WORKFLOW_ENGINE.md) | Workflow Engine — 结构化交付流水线 DSL + 执行流程 + CI Group (已验证) |
| [docs/design/PI_RPC_ALIGNMENT.md](./docs/design/PI_RPC_ALIGNMENT.md) | pi RPC CLI 对齐文档 (66 ✅ / 0 ❌ 全部对齐) |
| [docs/design/CLI_ARCHITECTURE.md](./docs/design/CLI_ARCHITECTURE.md) | CLI 三种执行场景设计：三场景分组验证用例 (已完成) |
| [docs/design/CLI_ROADMAP.md](./docs/design/CLI_ROADMAP.md) | CLI 落地路线图 (已完成) |
| [docs/design/CLI_PLAN.md](./docs/design/CLI_PLAN.md) | **CLI 完整落地方案（唯一入口）**：架构 + 路线图 + 验证用例 + checklist 合并 (已完成) |
| [docs/design/FAUX_PROVIDER.md](./docs/design/FAUX_PROVIDER.md) | FauxProvider 架构级 LLM Mock：FIFO 队列 + 工厂响应 + 流式分块，对标 pi (已实现 Phase 1) |
| [docs/design/RECORD_REPLAY.md](./docs/design/RECORD_REPLAY.md) | Record/Replay 录制回放：环境变量录制 + `--model replay/id` 回放，复用 FauxProvider (已实现 Phase 1) |
| [docs/design/SESSION_TREE.md](./docs/design/SESSION_TREE.md) | Session Tree（会话分支）：文件内分支 + leaf 指针 + only-append 回滚 (已实现) |
| [docs/design/SESSION_ISOLATION.md](./docs/design/SESSION_ISOLATION.md) | **会话隔离 + Session GC**：主会话默认 `<sid>.jsonl`（不再共享）+ 启动时 GC 清旧文件 + distillation 并发修复 (已验证) |
| [docs/design/MCP_SYSTEM.md](./docs/design/MCP_SYSTEM.md) | MCP 系统：rmcp 1.x + 方案 C 共享池 + 权限控制 + resources/prompts + 热更新 (Phase 1-4 全部实现) |
| [docs/design/CONFIG_DIMENSIONS.md](./docs/design/CONFIG_DIMENSIONS.md) | 配置与数据维度分析：5 类存储划分 + 组件归属全表 + worktree 副本预期 + StorageContext 统一抽象 + 新扩展开发指南 (已实现) |
| [docs/design/FILE_SNAPSHOT.md](./docs/design/FILE_SNAPSHOT.md) | File Snapshot：双路快照 + parented `step-snapshot` + tree-hash restore，restore_files + --restore-code 联动回滚，不遵守 .gitignore (已验证) |
| [docs/design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md](./docs/design/FILE_SNAPSHOT_REVIEW_ALIGNMENT.md) | File Snapshot & Review 对齐清单：ION vs pi 全维度对比 + tree 快照模型升级路线 + per-file 审批 + 4 步执行计划 (已实现步骤 1-4，e2e 待补) |
| [docs/design/MESSAGE_RETRIEVAL_DESIGN.md](./docs/design/MESSAGE_RETRIEVAL_DESIGN.md) | 消息拉取 UI 设计规格：TypeScript 接口定义 + 6 种 UI 风格 + 3 层数据架构 (设计定稿) |
| [docs/design/SOFT_DELETE_COMPACT.md](./docs/design/SOFT_DELETE_COMPACT.md) | 软删除/软压缩内核机制：mark_deleted/summarized/restore + on_context 时序 (已实现) |
| [docs/testing/MESSAGE_RETRIEVAL_CASES.md](./docs/testing/MESSAGE_RETRIEVAL_CASES.md) | 消息拉取 CLI 用例集：9 接口 + 12 Group A-L + 分页/视点/过滤/血缘 (设计定稿+已实现) |
| [docs/design/MEMORY_ACTIVE.md](./docs/design/MEMORY_ACTIVE.md) | Memory Active — V0.2 主动注入（on_input→on_context 自动检索全局库）+ 自动整理（去重/归档/大纲索引）+ bigram 中文分词 (已完成) |
| [docs/design/MEMORY_V2_PROCESSING.md](./docs/design/MEMORY_V2_PROCESSING.md) | Memory V0.2 会话加工 — SessionEnd 自动 LLM 提炼精华（替代原样存）+ 去重 + entities 铺路 (已完成) |
| [docs/design/SELF_EVOLUTION.md](./docs/design/SELF_EVOLUTION.md) | **自我进化闭环** — evolver agent + worktree + Apple Container 双重隔离 + ION 子实例改代码 + 测试 + 开 PR（开发中） |
| [docs/design/GOAL_SUPERVISOR.md](./docs/design/GOAL_SUPERVISOR.md) | **Goal Supervisor** — 证据驱动的目标闭环（on_gate_check + 6 道防线 + 日志 + 进化系统）+ A→B 任务规格 (B1 已完成) |
| [docs/design/DEV_SERVER_DETECTOR.md](./docs/design/DEV_SERVER_DETECTOR.md) | **Dev Server Detector** — bash 启动 dev server 时自动检测端口（stdout 扫描 + 探活兜底）+ on_system_prompt 注入 `<dev_servers>` XML（待定） |

### 使用指南（docs/guides/）

| 文档 | 内容 |
|------|------|
| [docs/guides/CLI_USAGE.md](./docs/guides/CLI_USAGE.md) | CLI 标准用法：RPC / Subscribe / Extension RPC / Tool RPC 完整速查 (已验证) |
| [docs/guides/AGENT_GUIDE.md](./docs/guides/AGENT_GUIDE.md) | **Agent 系统指南**：.md 格式 / 内置 Agent / 工具白名单黑名单 / MCP 工具交互 / 多智能体编排 / spawn_worker 透传 / CLI 测试 (已完成) |
| [docs/guides/MCP_USAGE.md](./docs/guides/MCP_USAGE.md) | **MCP 用法指南**：配置(stdio/http) / CLI 命令 / LLM 工具命名 / Agent 工具限制 / 权限规则 / 自动重连 / 故障排查 (已完成) |
| [docs/guides/PERMISSION_USAGE.md](./docs/guides/PERMISSION_USAGE.md) | **权限系统指南**：规则配置 / stored decisions / CommandGuard / CLI 命令 / Agent 工具限制 (已完成) |
| [docs/guides/FILE_SNAPSHOT_USAGE.md](./docs/guides/FILE_SNAPSHOT_USAGE.md) | **File Snapshot 指南**：启用配置 / 5 个 RPC / restore_files / --restore-code / 审批工作流 (已完成) |
| [docs/guides/RECORD_REPLAY_USAGE.md](./docs/guides/RECORD_REPLAY_USAGE.md) | **Record/Replay 指南**：录制(ION_RECORD) / 回放(--model replay/id) / ion recordings / 安全防护 (已完成) |
| [docs/guides/DEPLOY_ARCH.md](./docs/guides/DEPLOY_ARCH.md) | 部署架构 — 场景 + CLI 验证 |
| [docs/guides/SERVER_DEPLOY.md](./docs/guides/SERVER_DEPLOY.md) | **Linux 服务器部署指南**：CI 发 Release / 单二进制安装 / supervisord 守护场景三 / 7 个踩坑记录 / 资源实测 (已验证) |
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
| `src/bin/ion.rs` | 单一 CLI 入口 (45+ 参数)。`--mode rpc` 分支进入 worker 模式 |
| `src/worker_rpc.rs` | Worker RPC 实现 (~120 命令)。host 通过 `current_exe() + --mode rpc` spawn 自身创建 worker 子进程，对齐 pi 的 `pi --mode rpc` |
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
| `src/monitor_extension.rs` | MonitorExtension（单例扩展，场景 3 定时脚本监控→触发 LLM 对话） |
| `src/goal_supervisor_extension.rs` | GoalSupervisorExtension（证据驱动目标闭环：on_gate_check + 6 道防线 + 日志，[详情](./docs/design/GOAL_SUPERVISOR.md)） |
| `src/goal_evolver.rs` | Goal Evolver（日志分析进化：3 维度分析 + Issue 计划 + run_once，[详情](./docs/design/GOAL_SUPERVISOR.md §8)） |
| `src/lsp_extension.rs` | LSP Extension（多语言诊断：cargo check / tsc / go vet / py_compile / HTML 标签匹配） |
| `src/tool_loop_detector.rs` | Tool Loop Detector（防 LLM 重复调同一工具死循环） |
| `src/auto_session_title.rs` | Auto Session Title（首轮启发式标题生成） |
| `src/rules_engine.rs` | **Rules Engine**（项目规则注入：`.ion/rules/*.md` frontmatter glob 匹配 → on_system_prompt 注入 XML） |
| `src/learning_extension.rs` | **Learning Extension** Phase 2（session 分析：has_write_operations + redact_messages + should_extract） |
| `src/skill_distillation.rs` | **Skill Distillation** Phase 3（LLM 提炼技能：run_skill_distillation + summarize_args + resolve_session_file） |
| `src/secret_detector.rs` | **Secret Detector** Phase 1（API key/token 检测 + redact：detect_secrets + scan_known_prefixes + redact_secrets） |
| `src/agent/plan_extension.rs` + `src/agent/plan_tool.rs` | 内置 plan 工具（plan_enter/exit/add/list/done/approve + strict_mode 强制审批）|
| `src/hooks/` | Hooks 系统：HooksConfig + HookExtension + 5 handler 执行引擎（command/http/prompt/agent/mcp_tool），[详情](./docs/design/HOOKS_AND_OUTLINE_SYNC.md) |

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
    ❌ 无 UI 交互通道（权限拦截后无法放行，建议放开权限或用 .ion/settings.json 预配 allow 规则）
```

> **⚠️ 场景 2 权限注意事项**
>
> 场景 2 没有 socket，无法接收外部命令。如果权限规则 deny 了 agent 需要的操作（如 file.write），
> agent 会被卡住——既不能执行，也没有人能通过 `ion rpc` 放行。
>
> **建议**：在场景 2 下，通过 `.ion/settings.json` 预先配好 allow 规则（如 `allow file.write src/*`），
> 避免运行时出现 UI 交互需求。需要动态权限管理的场景请用场景 3（`ion serve`）。

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
ion "hello"              → 用户入口（场景 1：直接执行）
ion --host "hello"       → 带 host 能力的入口（场景 2：快速编排）
ion serve                → 常驻服务入口（场景 3：常驻服务）
ion --mode rpc           → 内部 Worker 子进程 (JSONL over stdin/stdout)
                            host 通过 current_exe() spawn 自身进入此模式
                            对齐 pi 的 `pi --mode rpc`，不再有独立 ion-worker 二进制
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


## 当前状态

> **完整状态快照**（功能清单 + 测试统计 + 路线图 + A→B 教程）已外移到 [docs/STATUS.md](./docs/STATUS.md)。
>
> 快速概览（2026-08-08 实测）：
> - **代码规模**：99,682 行 Rust（src 82,912）
> - **lib 测试**：1013 passed / 2 failed（2 个 hooks 测试逻辑缺陷，非产品 bug，待修）
> - **已完成**：核心内核 + 15+ 扩展系统 + 三场景引擎 + A→B 自进化
> - **HTML Export**：ION 自有单文件离线模板 + active branch 完整有序 `sourceEntries` + Flow Summary + Timeline/正文完整映射；目录展示 17 种固定 Entry、25 种已识别内置 Custom 与当前会话实际类型，运行时 Extension Custom 统一显示为 `Custom` 并保留来源、LLM 上下文与实时 UI 受众；Hook 归组、Compaction 与 parented File Snapshot 独立卡片；仅当隐藏正文超过 3 行时折叠（`tests/export_ci.sh` 54/54）
> - **PreToolUse 拒绝闭环**：拒绝转错误 ToolResult、Agent 继续、Hook 审计与 toolCallId/当前分支关联、SessionIndex 准确计数，导出类型目录保留 Hook/Extension 来源（Harness 1/1 + `tests/hooks_pretool_deny_ci.sh` 8/8）
>
> 历史改动看 `git log`，功能设计看 `docs/design/`，每个功能的测试看对应 `tests/*_ci.sh`。

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
cargo build --bin ion              # 单一二进制（含主 CLI + worker 模式）
cargo test --lib                   # 核心逻辑测试（实测 1013 passed / 2 failed，2026-08-06）
cargo test --test unit_rpc_test     # RPC 协议测试 (U1-U20)
cargo test                          # 全部测试
```

### target 目录清理

Rust 编译产物体积增长快，建议定期清理：

```bash
du -sh target/          # 查看大小（通常 2-10GB）
cargo clean             # 全删（下次全量编译）
rm -rf target/debug/    # 只删 debug（保留 release）
```

## 环境配置

```bash
ion config set api-key "sk-xxx"    # 存到 ~/.ion/auth.json (权限 600)
ion config set default-model glm-5.2
ion "hello"                        # 直接运行
```

自我进化 Container 环境详见 [docs/design/SELF_EVOLUTION.md](./docs/design/SELF_EVOLUTION.md)。
