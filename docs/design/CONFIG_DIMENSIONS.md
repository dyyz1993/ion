# 配置与数据维度分析（CONFIG_DIMENSIONS）

> **状态：设计稿** — 架构级梳理，发现 5 个设计缺口待修复

## 何时使用这个文档

- **触发场景**：讨论任何"项目级"功能、worktree 隔离、配置放哪个目录、新组件该挂什么维度
- **参考样本**：本文档是 AGENTS.md「项目级统一语义」的展开论证

---

## 概览

ION 有**全局 / 项目维度 / 仓库内 / Session / 单例**五种存储维度。一个组件该挂哪个维度，取决于它的内容**是否适合 git 追踪**、**是否需要 worktree 副本**、**是否跨 session 共享**。

核心结论（详见 §3）：

| 维度 | 存储位置 | git 追踪 | worktree 行为 | 适合放什么 |
|------|---------|---------|--------------|-----------|
| **① 全局** | `~/.ion/` | — | 天然全局 | 通用配置（provider/auth/全局记忆） |
| **② 项目维度** | `~/.ion/projects/<project_key>/` | ❌ 不依赖 | 主仓库和 worktree **共享**（同 key） | 含本地路径/密钥的项目配置（MCP/tier models） |
| **③ 仓库内** | `<project>/.ion/` | ✅ 适合 | 靠 git checkout 同步（被 gitignore 则丢） | 纯文本、团队共享内容（agent .md/skill .md） |
| **④ Session** | `~/.ion/agent/sessions/<cwd_hash>/` | ❌ | 每 session 独立（worktree 各自新会话） | 会话历史、注入记录 |
| **⑤ 单例** | `~/.ion/agent/global-memory.db` 等 | ❌ | 跨 worker/项目共享一份 | 全局记忆 DB、会话索引 |

---

## 1. 用户视角的 worktree「副本预期」

用户开一个 worktree 干活时，直觉预期是：**这是一个完整的工作副本，原来有什么功能，这边也有什么功能**。

| 用户期望 | 当前实现 | 状态 |
|---------|---------|------|
| worktree 里能用到项目的 Agent（`reviewer.md` 等） | worktree 无 `.ion/agents/`，读不到项目 Agent | ❌ **丢失** |
| worktree 里能用到项目的 Skill / WASM 扩展 | worktree 无 `.ion/skills/` `.ion/extensions/` | ❌ **丢失** |
| worktree 里项目的权限规则（禁止删 `.env` 等）仍生效 | worktree 无 `.ion/settings.json`，仅全局规则生效 | ❌ **丢失** |
| worktree 里 provider/model 配置和主仓库一致 | `ION_PROJECT_ROOT` 让 config 正确回源 | ✅ 正常 |
| worktree 里能查到主仓库改过的文件快照 | `project_key` 相同，file-snapshot 共享 | ✅ 正常 |
| worktree 里能查到全局记忆 | global-memory.db 是全局的 | ✅ 正常 |
| worktree 里 MCP server 配置可用 | MCP 配置设计为放 ② 项目维度（不依赖 git） | 🆕 设计中 |

**结论**：worktree 隔离的目标是**代码分支隔离**（git 干得的事），但 ION 的很多配置/扩展也跟着 git 走（③ 仓库内），导致 worktree 场景下功能残缺。只有把"含本地内容、不该提交 git 的配置"放到 ② 项目维度，才能满足副本预期。

---

## 2. 三类存储划分（决策准则）

### 2.1 判断流程

```
新增一个配置/数据项时：

Q1: 它是跨所有项目通用的吗？（如默认 provider、全局 API key）
  └─ 是 → ① 全局 (~/.ion/)

Q2: 它含本地路径 / 本地 URL / 密钥，不该提交 git 吗？
     （如 MCP command 路径 / 本地 HTTP URL / Bearer token / 本地 tier models）
  └─ 是 → ② 项目维度 (~/.ion/projects/<project_key>/)

Q3: 它是纯文本、适合团队共享的吗？（如 agent prompt .md / skill .md / permissions rules）
  └─ 是 → ③ 仓库内 (<project>/.ion/)

Q4: 它是单次会话的临时数据吗？（如对话历史 / 注入记录）
  └─ 是 → ④ Session (~/.ion/agent/sessions/)

Q5: 它必须跨 worker/项目共享一份吗？（如全局记忆索引）
  └─ 是 → ⑤ 单例 (~/.ion/agent/global-memory.db)
```

### 2.2 三类配置的合并优先级（高 → 低）

```
环境变量 / CLI flag（如 ION_API_KEY、--remote）
    ↓ 覆盖
② 项目维度配置  ~/.ion/projects/<project_key>/config.json
    ↓ 覆盖
③ 仓库内配置    <project>/.ion/config.json
    ↓ 覆盖
① 全局配置      ~/.ion/config.json
    ↓ 覆盖
内置默认值
```

### 2.3 为什么 ② 不放进 ③（仓库内）

MCP 配置常含：
- 本地命令路径：`/Users/xxx/tools/kb-mcp`（每台机器不同）
- 本地 HTTP URL：`http://localhost:8080/mcp`（每个环境不同）
- Bearer token / API key：不该提交 git

如果放进 ③，一旦 `.ion/` 被 gitignore（实测主仓库的 `.gitignore` 就含 `.ion/`），worktree 就彻底读不到。放 ② 后，`<project_key>` 用 git common dir 求解，**主仓库和所有 worktree 算出同一个 key → 天然共享**。

### 2.4 `<project_key>` 算法（统一入口 `paths::project_key_git`）

```
git rev-parse --git-common-dir
    → /ion/.git                 （主仓库：相对路径，canonicalize 后归一）
    → /ion/.git                 （worktree：git 直接返回主仓库共享目录）
取 canonicalize 后路径的 hash → <project_key>
```

> **实现位置**：`src/paths.rs::project_key_git()` + 辅助函数 `main_git_dir()`。
>
> **2026-07-12 重构**：从 `--absolute-git-dir` + 手动裁剪 `/worktrees/` 字符串约定，改为 `--git-common-dir`（git 官方维护的共享目录）。canonicalize 是因为 `--git-common-dir` 在主仓库返回相对 `.git`、在 worktree 返回绝对路径，必须归一才能保证 hash 一致。4 个单元测试全过（含 worktree 一致性）。

⚠️ **当前 `project_key_git` 只输出 hash，丢弃了原始路径**。要做 ② 配置定位，需要抽成 `git_project_root(cwd) -> Option<PathBuf>` 同时返回路径和 hash（见 §6 缺口 #2）。

---

## 3. 组件维度归属全表

### 3.1 配置类（config / settings）

| 配置项 | 当前位置 | 当前维度 | worktree 场景 | 正确维度 | 缺口 |
|--------|---------|---------|--------------|---------|------|
| `default_provider` / `default_model` / `base_url` | `~/.ion/config.json` + `<project>/.ion/config.json`（merge_project 覆盖） | ①③ 混合 | ✅ 靠 `ION_PROJECT_ROOT` 回源 | ①③ | — |
| `api_key` / `provider_api_keys` | `~/.ion/auth.json` | ① | ✅ 全局 | ① | — |
| `extensions.{name}.enabled` | `~/.ion/config.json` | ① | ✅ 全局 | ① | **merge_project 不合并 extensions**（项目级写了无效） |
| `tier_models` | `~/.ion/config.json` | ① | ✅ 全局 | ①②（② 支持项目级覆盖） | **merge_project 不合并 tier_models** |
| `runtime`（backends/routes/command_guard） | `~/.ion/config.json` | ① | ✅ 全局 | ① | **merge_project 不合并 runtime 子字段**（除 default_mode） |
| `runtime.default_mode` | 特殊处理 | ②（不从全局继承） | ✅ | ② | — |
| **MCP servers**（设计稿） | 设计为 `~/.ion/projects/<key>/config.json` | ② | 🆕 设计为共享 | ② | 待实现 |
| permissions rules | `~/.ion/settings.json` + `<project>/.ion/settings.json` | ①③ | ❌ worktree 读不到项目级 | ③（团队共享规则）+ ①（兜底） | **缺 worktree 回源** |

### 3.2 组件/扩展数据类

| 组件 | 数据位置 | 当前维度 | worktree 场景 | 正确维度 | 缺口 |
|------|---------|---------|--------------|---------|------|
| **Agent**（`.md`） | `~/.ion/agent/agents/` + `<project>/.ion/agents/` | ①③ | ❌ 项目级丢失 | ③（纯文本，适合 git） | **缺 worktree 回源** |
| **Skill**（`.md`） | `~/.ion/agent/skills/` + `<project>/.ion/skills/` | ①③ | ❌ 项目级丢失 | ③ | **缺 worktree 回源** |
| **WASM 扩展**（`.wasm`） | `~/.ion/agent/extensions/` + `<project>/.ion/extensions/` | ①③ | ❌ 项目级丢失 | ③ | **缺 worktree 回源** |
| WASM global data | `~/.ion/agent/extensions-data/<ext>/` | ① | ✅ 全局 | ① | — |
| WASM project data | `~/.ion/agent/project-data/<cwd_hash>/<ext>/` | ⚠️ **bug**：用 cwd hash 而非 project_key | ❌ worktree 独立（不共享） | ②（应改用 project_key） | **维度归属错误** |
| WASM project_local data | `<project>/.ion/<ext>/` | ③ | ⚠️ 靠 git checkout | ③ | — |
| WASM session data | `sessions/<cwd_hash>/data/<sid>/<ext>/` | ④ | ✅ session 隔离 | ④ | — |
| **Permission 内核** | 内存（每 worker 一份） | 单例（进程内） | ✅ 进程内 | — | — |
| **Memory v0.1**（项目级） | `~/.ion/agent/project-data/<cwd_hash>/memory/` | ⚠️ **bug**：用 cwd hash | ❌ worktree 独立 | ② | **维度归属错误** |
| **GlobalMemory** | `~/.ion/agent/global-memory.db` | ⑤ 单例 | ✅ 跨 worker 共享 | ⑤ | — |
| **File Snapshot** | `~/.ion/file-store/<project_key>/` | ②（正确） | ✅ worktree 共享 | ② | — |
| **Bash** 后台进程 | `<tmp>/ion-bash/processes.json` | 全局临时 | ✅ 全局 | — | — |
| **Session** 文件 | `~/.ion/agent/sessions/<cwd_hash>/` | ④ | ✅ session 隔离 | ④ | — |
| **SessionIndex** | `~/.ion/agent/sessions.index.json` | ⑤ 单例 | ✅ 全局索引 | ⑤ | — |
| **Record/Replay** | `~/.ion/recordings/<id>/` | ① | ✅ 全局 | ① | — |
| **Worktree 目录** | `~/.ion/worktrees/<rand8>/<proj>/` | 临时 | — | — | 回收时删，分支保留 |

---

## 4. Worktree 场景详解

### 4.1 Worktree 创建流程

```
ion --host "用 worktree 开发"
    ↓
coordinator 调 spawn_worker(worktree=true)
    ↓
WorkerRegistry::create_worker (worker_registry.rs:153)
    ↓
create_worktree_advanced (worker_registry.rs:1917)
    ├─ 路径: ~/.ion/worktrees/<rand8>/<project_name>/
    ├─ 分支: ion-<session_id> 或 ion-worker-<ts>
    ├─ 命令: git -C <主仓库> worktree add <dir> -b <branch>
    └─ ⚠️ 不复制任何配置文件，内容完全由 git checkout 决定
    ↓
spawn 子 worker (worker_registry.rs:268-297)
    ├─ current_dir = worktree 路径（不是主仓库）
    ├─ ION_PROJECT_ROOT = 主仓库路径（仅此处注入）
    ├─ ION_RUNTIME_OVERRIDE 透传
    └─ ION_FAUX_* / ION_RECORD 透传
```

**关键事实**：主仓库的 `.gitignore` 含 `.ion/`（实测确认），因此 worktree 目录里**没有 `.ion/` 目录**。

### 4.2 子 Worker 启动后的配置读取路径

| 功能 | 代码读取的 base | 行号 | worktree 里实际读到的 |
|------|---------------|------|---------------------|
| `IonConfig`（provider/model/runtime） | `ION_PROJECT_ROOT`（主仓库） | `config.rs:534` | ✅ `/主仓库/.ion/config.json` |
| WASM 扩展发现 | `current_dir()`（worktree） | `ion_worker.rs:241` | ❌ `<worktree>/.ion/extensions/` 不存在（仅全局生效） |
| 项目级 Agent | `current_dir()`（worktree） | `agent_config.rs:146` | ❌ `<worktree>/.ion/agents/` 不存在（fallback 到全局） |
| 项目级 Permission rules | `project_root`=worktree | `permission_extension.rs:88` | ❌ `<worktree>/.ion/settings.json` 不存在（仅全局生效） |
| File Snapshot | `current_dir()` 但 project_key 自愈 | `object_store.rs:216` | ✅ project_key 相同，共享存储 |
| 项目级 Memory | `current_dir()`（worktree cwd hash） | `ion_worker.rs:195` | ⚠️ worktree 独立（不符合副本预期） |
| GlobalMemory | 固定路径 `~/.ion/agent/` | `global_memory.rs:159` | ✅ 跨 worker 共享 |

### 4.3 根因分析：`ION_PROJECT_ROOT` 只被一处消费

```bash
grep -rn "ION_PROJECT_ROOT" src/
# config.rs:534          → load_project() 读取（唯一消费点）
# worker_registry.rs:278 → spawn 时注入（设置点）
```

**只有 `config.rs` 用了 `ION_PROJECT_ROOT`**。其余所有项目级资源读取（WASM 发现、Agent、Skill、Permission、Memory）都直接用 `std::env::current_dir()`（= worktree 目录），导致它们在 worktree 场景下读不到项目级资源。

这是当前 worktree 隔离最大的设计缺口。

---

## 5. 发现的设计缺口（按优先级）

### 缺口 #1：`merge_project()` 只合并 3 个字段（HIGH）— ✅ 已修复

**现象**：`config.rs:546-557` 的 `merge_project()` 只覆盖 `default_provider` / `default_model` / `base_url`，其余字段（`extensions` / `tier_models` / `runtime` 子字段 / `providers` / `mcp_servers`）在项目级配置里写了**完全无效**。

**影响**：用户在 `<project>/.ion/config.json` 写的 `extensions.file-snapshot.enabled = true` 不生效。

**修复**：`merge_project` 已改成深度合并所有字段（HashMap 按 key 合并、Option 按需覆盖、Vec 非空替换）。9 个单元测试（`merge_tests::a1-a8b`）覆盖 Group A 全部 case。

**遗留问题（serde 默认值陷阱）**：`tier_models` 字段标了 `#[serde(default = "default_tier_models")]`，项目级 config 从文件反序列化时，**即使用户没写 tier_models，serde 也会填上默认值**（fast/pro/max）。merge 时这些"默认值"会覆盖全局的显式配置。影响：如果全局配了 `tier_models.fast = "custom/model"`，项目级 config 文件完全没提 tier_models，merge 后 fast 会被项目级的默认值 `deepseek/deepseek-v4-flash` 覆盖。后续需考虑用 `Option<HashMap>` 或自定义反序列化区分"未设置"和"显式空"。

### 缺口 #2：`ION_PROJECT_ROOT` 只被 config 消费（HIGH）— ✅ 已修复

**现象**：WASM 发现、Agent、Skill、Permission、Memory 都用 `current_dir()` 而非 `ION_PROJECT_ROOT`，worktree 场景下项目级资源全部丢失。

**影响**：worktree 里用不到项目级 Agent/Skill/WASM/Permission rules。

**修复**：
- 新增 `paths::project_root_for_config()` —— 统一解析项目根（优先 `ION_PROJECT_ROOT`，回退 `current_dir()`）
- 5 处消费点改用 `config_root`（区别于 session 用的 `worker_cwd`）：
  - WASM 扩展发现（`ion_worker.rs` 原 241 行）
  - Memory Store + MemoryExtension（`ion_worker.rs` 原 195/386 行）—— worktree 共享记忆
  - PermissionExtension（`ion_worker.rs` 原 409 行）—— 读主仓库 `.ion/settings.json`
  - 项目级 Skills（`get_skills` RPC）—— 读主仓库 `.ion/skills/`
  - 项目级 Agent（`agent_config.rs::project_agents_dir()`）—— 读主仓库 `.ion/agents/`
- **保持不变**：session 文件、file-snapshot 用 `worker_cwd`（session 按 cwd 隔离是设计意图，file-snapshot 的 project_key 已自愈）

### 缺口 #3：两套 project_key 体系不统一（MEDIUM）— ✅ 已修复（核心抽取）

**现象**：
- file-snapshot 用 **git common dir hash**（`object_store.rs:216`）→ worktree 共享 ✅
- project-data / session / WASM project data 用 **cwd 路径 hash**（`paths.rs:encode_path`）→ worktree 独立 ❌

**修复**：
- 抽出 `paths::git_project_root(cwd) -> Option<PathBuf>` —— 从 git-dir 反推主仓库根（worktree → 主仓库）
- 抽出 `paths::project_key_git(cwd) -> String` —— git common dir hash，主仓库和 worktree 一致
- `file_snapshot::object_store::project_key` 委托给 `paths::project_key_git`（行为不变，统一入口）
- 4 个新单元测试验证（`git_project_root_returns_main_repo` / `git_project_root_worktree_shares_main` / `project_key_git_worktree_consistency` / `project_root_for_config_env_and_cwd_fallback`）
- **2026-07-12 算法升级**：`project_key_git` 内部从 `--absolute-git-dir` + 手动裁剪 `/worktrees/` 改为 `--git-common-dir`（git 官方共享目录，不依赖路径字符串约定）+ `canonicalize` 归一。新增辅助函数 `main_git_dir(cwd) -> Option<String>`。

**遗留**：project-data / WASM project data 的 `encode_path`（cwd hash）尚未改成 `project_key_git`。这些通过缺口 #2 的 `config_root` 回源已部分缓解（Memory 现在用 config_root），但彻底统一需后续把 `project_data_dir` 的 key 源也切换。

### 缺口 #4：settings.json 全局路径不一致（LOW）— ✅ 已修复

**现象**：
- `permission_extension.rs:90` 用 `~/.ion/settings.json`（硬编码）
- `paths.rs:159` 的 `settings_path()` 返回 `~/.ion/agent/settings.json`（死代码，无调用者）

**修复**：`settings_path()` 改为返回正确的 `~/.ion/settings.json`（与 permission_extension 实际使用一致）；`permission_extension` 改用 `paths::settings_path()` 取代硬编码。

### 缺口 #5：worktree 路径文档与实现不一致（LOW）— ✅ 已修复

**现象**：
- `paths.rs:46` 注释写 `~/.ion/worktrees/<repoName>-<safeBranch>/`
- 实际 `worker_registry.rs:1933` 用 `~/.ion/worktrees/<rand8>/<project_name>/`

**修复**：更新 `paths.rs:46` 注释为 `<rand8hex>/<projectName>/`，与实现一致。

---

## 6. MCP 配置的维度归属结论

基于以上分析，**MCP 配置应归 ② 项目维度**：

| MCP 配置内容 | 是否含本地内容 | 维度归属 |
|-------------|--------------|---------|
| `command: "/Users/xxx/tools/kb-mcp"` | ✅ 本地路径 | ② |
| `url: "http://localhost:8080/mcp"` | ✅ 本地 URL | ② |
| `headers: { Authorization: "Bearer xxx" }` | ✅ 密钥 | ② |
| `disabled: false` | ❌ 布尔值，但不单独存 | ② |

**存放路径**：`~/.ion/projects/<project_key>/config.json` 的 `mcp_servers` 字段

**与全局的关系**：项目维度 `mcp_servers` 与全局 `mcp_servers` 按 server name 浅合并（同名覆盖、不同名保留）。

**worktree 场景**：主仓库和所有 worktree 算出同一个 `<project_key>` → 共享同一份 MCP 配置，**不依赖 git 同步**。

---

## 7. 后续工作

| # | 内容 | 优先级 | 依赖 |
|---|------|-------|------|
| 1 | 抽出 `git_project_root(cwd) -> Option<PathBuf>`（从 file-snapshot 的 project_key 重构，返回路径 + hash） | HIGH | 无（缺口 #3 前置） |
| 2 | 修复 `merge_project()` 深度合并所有字段 | HIGH | 无（缺口 #1） |
| 3 | 5 处项目级读取点改用 `ION_PROJECT_ROOT` / `git_project_root` | HIGH | #1（缺口 #2） |
| 4 | 统一 project_key 体系（project-data / WASM project data 改用 git common dir hash） | MEDIUM | #1（缺口 #3） |
| 5 | settings.json 路径统一 + worktree 路径文档修正 | LOW | 无（缺口 #4/#5） |
| 6 | MCP Phase 1 实现（配置放 ② 项目维度） | HIGH | #1 |

---

## 8. 验收用例（缺口修复 + 维度统一后逐条验证）

> **用途**：每个缺口修复后，用对应 Group 的 case 验证。全部通过 = 维度设计真正落地。
> **脚本**：`tests/config_dimensions_ci.sh`（实现后编写），每条 case 可独立运行。
> **前置**：每条 case 默认在干净的测试仓库里跑，测试仓库的 `.gitignore` **含 `.ion/`**（还原真实场景）。

### Group A — 缺口 #1：merge_project 深度合并

验证目标：项目级 config 里写的字段，不再被静默忽略。

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| A1 | extensions 合并 | 全局不配；项目级写 `extensions.file-snapshot.enabled=true`；启动 worker | `is_extension_enabled("file-snapshot")` 返回 **true**（当前会返回 false —— bug） |
| A2 | extensions 覆盖 | 全局 `extensions.memory.enabled=true`；项目级 `extensions.memory.enabled=false` | 内存扩展在项目里被**关闭** |
| A3 | tier_models 合并 | 全局只配 `fast`；项目级补 `pro` | 两层合并后 `fast` + `pro` 都在 |
| A4 | tier_models 覆盖 | 全局 `fast→deepseek/flash`；项目级 `fast→deepseek/pro` | 项目里 `fast` 解析到 `pro` |
| A5 | runtime.backends 合并 | 全局配 backend `local`；项目级补 backend `remote-a` | 项目里 `get_backends` 返回 2 个 |
| A6 | runtime.command_guard 合并 | 全局 mode=whitelist；项目级 mode=open | 项目里 guard mode=open |
| A7 | providers 合并 | 全局配 provider `zai`；项目级补 `anthropic` | `list_models` 能看到两家 |
| A8 | api_key 不污染 | 全局 `api_key=global-key`；项目级不写 | 合并后 api_key 仍是 global-key（项目级不该清空全局的） |

### Group B — 缺口 #2：worktree 回源（ION_PROJECT_ROOT 全消费）

验证目标：worktree 场景下，项目级资源（Agent/Skill/WASM/Permission）都能读到主仓库的。

**前置**：主仓库 `.ion/` 被 gitignore（实测确认）。worktree 由 `spawn_worker(worktree=true)` 创建，`ION_PROJECT_ROOT` 自动注入。

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| B1 | 项目 Agent 可达 | 主仓库放 `.ion/agents/custom.md`；worktree worker 调 `find_agent("custom")` | ✅ 找到（当前会 fallback 到全局 —— bug） |
| B2 | 项目 Skill 可达 | 主仓库放 `.ion/skills/proj-skill.md`；worktree worker 调 `get_skills` | 列表含 `proj-skill`，source 标注正确 |
| B3 | 项目 WASM 可达 | 主仓库放 `.ion/extensions/myext.wasm`；worktree worker 启动 | 扩展被加载（`get_extensions` 含 `myext`） |
| B4 | 项目 Permission rules 生效 | 主仓库 `.ion/settings.json` 配 `禁删 *.env`；worktree worker 调删 `.env` | **被 Deny**（当前仅全局规则生效 —— bug） |
| B5 | Permission 个人规则持久化位置 | worktree worker 里 `extension_rpc` 存一条 `scope=Project` 规则 | 存到主仓库 `.ion/settings.json`，**不**存到 worktree（防回收丢失） |
| B6 | config 仍正确 | 同 A 场景，但在 worktree 里跑 | 结果与主仓库一致（这条当前就过，作回归保护） |

### Group C — 缺口 #3：project_key 体系统一

验证目标：项目级数据（Memory / WASM project data）在 worktree 与主仓库间共享，而非各自一份。

**前置**：在主仓库跑一次产生数据，再到 worktree 查。

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| C1 | project_key 一致性 | 主仓库和其 worktree 各自算 `project_key(cwd)` | 返回**相同**的 16 位 hex（当前 file-snapshot 已对，作基线） |
| C2 | project-data 目录一致 | 主仓库算 `paths::project_data_dir(cwd,"x")`；worktree 算同一个 | 指向**同一个** `~/.ion/projects/<key>/x/`（当前不一致 —— bug） |
| C3 | Memory worktree 共享 | 主仓库存一条 memory；worktree worker 调 `memory_search` | **搜得到**（当前搜不到 —— 各自一份） |
| C4 | WASM project data 共享 | 主仓库 WASM 扩展写 project data；worktree 同扩展读 | **读到**主仓库写的数据 |
| C5 | Session 仍隔离 | 主仓库和 worktree 各自跑对话 | 会话文件**各自独立**（session 维度不变，这是设计意图） |

### Group D — 缺口 #4：settings.json 路径统一

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| D1 | 全局 settings 路径一致 | 读 `PermissionExtension` 用的全局 settings 路径 vs `paths::settings_path()` | **同一文件**（当前不一致 —— bug） |
| D2 | 热重载读对文件 | 改全局 settings 后 `extension_rpc reload` | 读到改动（当前可能读错文件） |

### Group E — 缺口 #5：worktree 路径文档对齐

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| E1 | 路径结构 | 创建 worktree 后看实际路径 | 与 `paths.rs:46` 注释**一致**（二选一：改代码 or 改注释） |

### Group F — MCP ② 项目维度（MCP Phase 1 验收）

验证目标：MCP 配置放对地方，worktree 能共享，gitignore 不影响。

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| F1 | 配置位置正确 | 写 MCP 配置后看实际落盘 | 在 `~/.ion/projects/<key>/config.json`，**不**在 `<project>/.ion/` |
| F2 | gitignore 无影响 | 主仓库 `.ion/` 被 gitignore；worktree 跑 `get_mcp_servers` | 仍读到项目维度配置（② 不依赖 git） |
| F3 | worktree 共享 | 主仓库配 MCP；worktree `get_mcp_servers` | 看到同一份（同 `<key>`） |
| F4 | 全局+项目合并 | 全局配 `kb`；项目维度配 `linter` + 覆盖 `kb` | 合并后含 `linter` + `kb`(被覆盖的值) |
| F5 | 不同项目隔离 | 两个不同 git 仓库的项目维度配置 | `<key>` 不同，互不干扰 |

### Group G — 维度归类正确性（回归基线）

验证目标：现有正确实现的组件不被回归破坏。

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| G1 | file-snapshot worktree 共享 | 主仓库改文件产生快照；worktree 调 `get_modified_files` | 能查到主仓库的快照（project_key 相同） |
| G2 | global-memory 跨 worker | worker A 存全局记忆；worker B 搜 | 搜得到 |
| G3 | session 隔离不变 | 主仓库和 worktree 各自对话 | 会话文件独立（④ 维度保持） |
| G4 | auth 全局 | worktree worker 读 API key | 从 `~/.ion/auth.json` 读到（① 全局） |

### 用例与缺口的对应关系

| Group | 验证的缺口 | case 数 | 全过 = |
|-------|-----------|--------|--------|
| A | #1 merge_project | 8 | 项目级 config 全字段生效 |
| B | #2 worktree 回源 | 6 | worktree 副本预期达成 |
| C | #3 project_key 统一 | 5 | 项目数据 worktree 共享 |
| D | #4 settings 路径 | 2 | 全局 settings 统一 |
| E | #5 路径文档 | 1 | 文档与实现一致 |
| F | MCP ② 维度 | 5 | MCP 配置维度正确 |
| G | 回归基线 | 4 | 现有正确实现不破坏 |
| **合计** | | **31** | **维度设计真正落地** |

---

## 决策索引

| 决策 | 章节 | 结论 |
|------|------|------|
| 配置分几类 | §2 | 5 类（全局/项目维度/仓库内/Session/单例） |
| MCP 配置放哪 | §6 | ② 项目维度（`~/.ion/projects/<key>/`） |
| 为什么不放仓库内 | §2.3 | 含本地路径/密钥，不该 git 追踪；worktree 读不到 |
| worktree 副本预期 | §1 | 应与主仓库功能一致，当前 3 项丢失 |
| project_key 用哪个 | §4.2 | git common dir hash（file-snapshot 已用，其余待统一） |
| 最大的缺口 | §5 | #2：ION_PROJECT_ROOT 只被 config 消费 |
