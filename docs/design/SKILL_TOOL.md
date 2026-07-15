# Skill 工具 — LLM 按需调用 设计文档

> **状态：待定** — 让 LLM 在对话过程中主动调用 skill，而不是只在启动时 `--skill` 注入。
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
| `fork` | skill 在隔离 subtask 中跑（不污染主会话） | 🔧 后续（需 subtask 内核原语） |

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
| `src/agent/tool.rs` | 新增 SkillTool struct + impl Tool | ~100 |
| `src/bin/ion_worker.rs` | 注册 SkillTool（扫描 skill 目录） | ~15 |
| `src/bin/ion.rs` | system prompt 加 skill 提示 | ~10 |
| `tests/skill_tool_ci.sh` | CLI 测试 | ~50 |
| **总计** | | **~175** |

## 4. CLI 测试指南

### Group A：skill 工具基本流程

```bash
# 准备测试 skill
mkdir -p .ion/skills
echo '---
name: test-skill
description: A test skill
---
# Test Skill
Do the thing.' > .ion/skills/test-skill.md

# A1 列出可用 skill（FauxProvider 驱动）
# 配 FauxProvider 让 LLM 调 skill list
ion rpc --session <sid> --method prompt --params '{"text":"列出可用的 skill"}'
# 验证输出包含 test-skill

# A2 加载 skill
ion rpc --session <sid> --method prompt --params '{"text":"加载 test-skill"}'
# 验证 skill 内容被注入
```

## 5. 并行开发注意事项

- **不依赖**其他 3 份文档，可独立并行开发
- 改动集中在 `tool.rs`（加一个 Tool） + `ion_worker.rs`（注册）+ `ion.rs`（prompt）
- 不改 Extension trait，不影响 hooks/权限系统
- 与 PERMISSION_STORE.md 互不干扰（改的文件不重叠）

## 6. 对标 pi

| 对比项 | pi | ION |
|--------|-----|-----|
| skill 工具 | ✅ `core/tools/skill.ts` | 🔧 本文档新增 |
| 发现目录 | ~/.pi/skills + project | ~/.ion/skills + project（对齐） |
| inject 模式 | ✅ | ✅ |
| fork 模式 | ✅（subtask） | 🔧 后续（需 subtask 原语） |
| frontmatter | name/description/trigger | 对齐 |
