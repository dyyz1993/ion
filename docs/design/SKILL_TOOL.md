# Skill 工具 — LLM 按需调用 设计文档

> **状态：已完成** — SkillTool 已实现（list / inject / fork 三种模式全部可用）。
> - **inject**（默认）：当前上下文加载，场景 1/2/3 都可用
> - **fork**：隔离子 Worker 执行，**仅场景 2/3 可用**（需要 host 引擎才能 spawn_worker）
>   - 场景 1（直接 `ion -p "..."`）调用 fork 会返回 fallback 提示，LLM 应改用 inject
>   - 场景 2/3（`ion --host` / `ion serve`）fork 正常工作：spawn_worker 起子任务
>   - **fork 子 Worker 用独立 session 文件 `<session_id>.jsonl`**
>   - **session header 含完整血缘关联**：`parentSession`（pi 兼容）+ `spawnMeta`（ION 扩展）
>     ```json
>     { "parentSession": "sess_xxx",
>       "spawnMeta": { "parentWorker": "wkr_xxx", "relation": "child", "spawnedBy": "skill_fork" } }
>     ```
>   - **skill 内容存到 session 文件**（custom entry `customType=system_prompt`），
>     export HTML 时恢复到顶层 `systemPrompt` 字段
>   - memory-agent（System 关系 Worker）也用独立 session 文件，不污染主 session
>
> **⚠️ 已知限制**：fork 子 Worker 的 message 在 agent.run 完成后才落盘。
> 如果 fork 任务超时被杀，子 Worker 的 message 会丢（但 header + systemPrompt 保留）。
> 解决方案（待做）：StreamingExtension 增量 save。
>
> 对齐 pi 的 `core/tools/skill.ts`。

---

## 何时使用这个文档

- 想让 LLM 根据任务需要自主选择加载哪个 skill
- 想让 skill 在运行时动态加入（不重启）
- 想让 skill 在隔离 subtask 中运行（fork context，不污染主会话）

**前置阅读**：无

---

## 1. 问题

ION 当前的 skill 系统（`src/bin/ion.rs:1298`）：
- `--skill <path>` 启动时加载，把 skill 内容拼到 system prompt
- 运行中不能加新 skill
- LLM 不能按需选择 skill（全在 prompt 里了，不管用不用得上）

**缺失**：LLM 不能在对话中主动说"我需要用 review skill"然后加载它。pi 有一个 `skill` 工具让 LLM 按需调用。

## 2. 设计

### 2.1 SkillTool — 新工具

**文件**：`src/agent/tool.rs`（加一个 Tool 实现）

```rust
/// Skill 工具 — 让 LLM 按需加载 skill
pub struct SkillTool {
    /// skill 根目录（~/.ion/skills/ + <project>/.ion/skills/）
    skill_dirs: Vec<PathBuf>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str { "skill" }

    fn description(&self) -> &str {
        "Load a skill by name. Skills provide specialized instructions and capabilities.\n\
         Use this when you need domain-specific guidance (e.g., code review, testing, deployment).\n\
         Available skills can be listed with skill_name='list'."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Name of the skill to load (e.g., 'code-review', 'testing'). Use 'list' to see available skills."
                },
                "context": {
                    "type": "string",
                    "enum": ["inject", "fork"],
                    "description": "How to apply the skill. 'inject' = add to current context (default). 'fork' = run in isolated subtask (not yet implemented)."
                }
            },
            "required": ["skill_name"]
        })
    }

    async fn execute(&self, args: Value, rt: &dyn Runtime) -> Result<String, AgentError> {
        let name = args.get("skill_name")?.as_str();
        let context_mode = args.get("context").and_then(|v| v.as_str()).unwrap_or("inject");

        // list 模式：列出可用 skill
        if name == "list" {
            return Ok(self.list_skills());
        }

        // 查找 skill 文件
        let skill_path = self.find_skill(name)?;
        let content = std::fs::read_to_string(&skill_path)?;

        // 解析 frontmatter + body
        let (frontmatter, body) = parse_skill_content(&content);

        // inject 模式：返回 skill 内容（agent loop 会注入到上下文）
        Ok(format!("Skill '{name}' loaded:\n\n{body}"))
    }
}
```

### 2.2 Skill 发现

扫描两个目录的 `.md` 文件：
- `~/.ion/skills/*.md` — 全局 skill
- `<project>/.ion/skills/*.md` — 项目级 skill

文件名（不含 .md）就是 skill 名。例如 `~/.ion/skills/code-review.md` → skill_name = "code-review"。

### 2.3 Skill 文件格式

```markdown
---
name: code-review
description: Perform a thorough code review
trigger: when user asks to review code
---

# Code Review Skill

## Steps
1. Read the changed files
2. Check for common issues (security, performance, style)
3. Provide structured feedback
...
```

frontmatter 的 `description` 用于 `list` 输出，`trigger` 提示 LLM 什么时候用这个 skill。

### 2.4 inject vs fork 模式

| 模式 | 行为 | 实现状态 |
|------|------|---------|
| `inject`（默认） | skill 内容注入当前会话上下文 | ✅ 本文档实现 |
| `fork` | skill 在隔离 subtask 中跑（不污染主会话） | ✅ 通过 spawn_worker 起子任务，skill 注入 system prompt（不被 compaction 压缩） |

`inject` 模式下，SkillTool 的返回值（skill body 文本）会被 agent 当作工具结果，LLM 下一轮就能看到它。

### 2.5 system prompt 提示

在 system prompt 里加一段提示 LLM 有 skill 工具可用：

```
You have access to a `skill` tool. Use it to load specialized capabilities when needed.
Available skills: code-review, testing, deployment, ...
Use skill_name='list' to see all available skills.
```

这段提示让 LLM 知道有哪些 skill 可选，但不预加载内容（省 token）。

## 3. 改动文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/agent/tool.rs` | 新增 SkillTool struct + impl Tool + frontmatter 解析 + 6 单元测试 | ~230 |
| `src/bin/ion_worker.rs` | 注册 SkillTool + system prompt 加 skill 提示（`build_skill_hint`） | ~55 |
| `tests/skill_tool_ci.sh` | CLI 测试（Group S + Group E，13 case） | ~200 |
| **总计** | | **~485** |

> system prompt 提示在 `ion_worker.rs` 的 `build_skill_hint()` 函数实现（不是 `ion.rs`），因为 Worker 启动时扫描 skill 目录拼提示更合适。

## 4. CLI 测试指南

### RPC 接口

skill 是一个 LLM 工具，通过 `call_tool` RPC 直接调用（不依赖 LLM 决策，确定性验证）：

**请求：**
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review"}}'
```

**请求参数（args 内）：**
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `skill_name` | string | 必填 | skill 名字，`list` 列出全部 |
| `context` | string | `"inject"` | `inject` 注入当前上下文 / `fork` 隔离 subtask（未实现） |

**响应 JSON（成功 - inject）：**
```json
{"success":true,"data":{"tool":"skill","output":"Skill 'code-review' loaded:\n\n# Code Review Skill\n..."}}
```

**响应 JSON（成功 - fork 未实现）：**
```json
{"success":true,"data":{"tool":"skill","output":"Skill 'code-review': 'fork' mode is not yet implemented..."}}
```

**响应 JSON（失败 - skill 不存在）：**
```json
{"success":false,"error":"call_tool skill: Tool call failed: skill 'ghost' not found in [...]"}
```

### Group S：skill 工具基本流程

#### S1 列出可用 skill
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"list"}}'
```
**验证点：**
- ✅ 列出全局（`~/.ion/agent/skills/`）+ 项目级（`<project>/.ion/skills/`）skill
- ✅ 每条含 name、source、description

#### S3 加载 skill（inject 模式）
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review"}}'
```
**验证点：**
- ✅ 返回 `Skill 'code-review' loaded:` + 正文
- ✅ frontmatter 被剥离（只返回 body）

#### S5 fork 模式（未实现）
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"code-review","context":"fork"}}'
```
**验证点：**
- ✅ 返回 `not yet implemented` 提示，`success=true`（不报错）

### Group E：边界场景

#### E1 无 skill 时
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"skill","args":{"skill_name":"list"}}'
```
**验证点：**
- ✅ 返回 `No skills available.`

### 自动化脚本

```bash
bash tests/skill_tool_ci.sh    # 13 case，FauxProvider 驱动，隔离 HOME + ION_AGENT_DIR
```

## 5. 并行开发注意事项

- **不依赖**其他 3 份文档，可独立并行开发
- 改动集中在 `tool.rs`（加一个 Tool） + `ion_worker.rs`（注册 + prompt）
- 不改 Extension trait，不影响 hooks/权限系统
- 与 PERMISSION_STORE.md 互不干扰（改的文件不重叠）
- ✅ 验证：与 PERMISSION_STORE 并行开发同时存在工作区时，SkillTool 的 6 单元测试 + 13 CLI 测试全过

## 6. 对标 pi

| 对比项 | pi | ION |
|--------|-----|-----|
| skill 工具 | ✅ `core/tools/skill.ts` | ✅ `src/agent/tool.rs` SkillTool |
| 发现目录 | ~/.pi/skills + project | `~/.ion/agent/skills/` + `<project>/.ion/skills/`（对齐） |
| inject 模式 | ✅ | ✅ |
| fork 模式 | ✅ | ✅ spawn_worker 起子任务，skill 注入 system prompt（不被 compaction 压缩） |
| frontmatter | name/description/trigger | name/description（trigger 解析但暂不用） |
| system prompt 提示 | ✅ | ✅ `build_skill_hint()`（列出名字，不预加载内容省 token） |

## 7. 验证结果（2026-07-15）

| 测试 | 数量 | 状态 |
|------|------|------|
| 单元测试（`skill_tests`，frontmatter 解析 + list/find 边界） | 6 | ✅ 全过 |
| CLI 测试（`skill_tool_ci.sh`，Group S 9 + Group E 1 + build/host/create 3） | 13 | ✅ 全过 |
| 全量 lib 测试（临时排除并行开发的 permission_extension） | 367 | ✅ 全过（361 baseline + 6 skill） |
| frontmatter | name/description/trigger | 对齐 |
