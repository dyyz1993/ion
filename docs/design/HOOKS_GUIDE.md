# Hooks 系统 使用指南

> **状态：开发中** — 本文档面向"想用 hooks 的人"（用户/QA/产品），只讲是什么/怎么用/怎么调，**不含任何实现代码**。
>
> - 想看实现规格（数据结构/handler 引擎/改动清单）→ [HOOKS_AND_OUTLINE_SYNC.md](./HOOKS_AND_OUTLINE_SYNC.md)
> - 想看 CLI 验证用例（Group A-H + 完整 JSON）→ [HOOKS_CLI_TEST.md](./HOOKS_CLI_TEST.md)

---

## 1. Hooks 是什么

一句话：**你写一个 `hooks.json`，声明"当某事件发生时执行什么动作"，存盘后立即生效，不用重启。**

### 解决什么问题

ION 的 Agent 在运行时会经历很多"关键节点"——用户提交输入、调用工具、子 agent 结束、会话压缩……之前你想在这些节点插入自定义逻辑（比如"用户提交前校验一下"、"工具调用后记个日志"），必须写 Rust/WASM 扩展，门槛很高。

Hooks 让你**用 JSON + shell 脚本就能扩展行为**，不用写 Rust。典型场景：

| 场景 | 怎么配 |
|------|--------|
| 用户提问前，注入项目文档大纲 | `UserPromptSubmit` → 跑个 shell 读 outline 输出文本 |
| 工具调用前，拦截危险命令 | `PreToolUse` → 跑个脚本检查，exit 2 阻断 |
| 子 agent 干完活后，自动同步大纲 | `SubagentStop` → 起一个 agent 去检查更新 |
| 会话压缩前，先备份关键信息 | `PreCompact` → 跑脚本存档 |
| agent 想停下时，强制再检查测试 | `Stop` → 脚本检查，exit 2 让 agent 继续 |

### 和已有"扩展"的关系

```
ION 的扩展能力，从重到轻：
  WASM 扩展（写 Rust 编译成 .wasm）    ← 最强，门槛最高
  内置 Rust 扩展（编译进内核）          ← 最强，需改内核
  ────────────────────────────────
  Hooks（配 JSON + 写 shell 脚本）     ← 轻量，本文档的主角
  ────────────────────────────────
  JSON 扩展（只拼 system prompt）       ← 最弱，门槛最低
```

Hooks 不是替代 WASM 扩展，而是**填补"想自定义一点行为但又不想写 Rust"的空白**。

---

## 2. 30 秒上手

### 第 1 步：创建配置

在你的项目根目录建 `.ion/hooks.json`：

```json
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "echo '注意：用户提交了问题'",
        "timeout": 3
      }
    ]
  }
}
```

### 第 2 步：生效了吗？

```bash
# 方式 1（推荐）：用项目自带的验证脚本，不依赖 Rust
bash scripts/hooks_test.sh validate .ion/hooks.json   # 校验格式
bash scripts/hooks_test.sh list                        # 看生效的 hooks

# 方式 2（将来）：ion 内置命令（待实现，API 与上面脚本一致）
# ion hooks validate .ion/hooks.json
# ion hooks list
```

**存盘即生效**——下次用户提问时，"注意：用户提交了问题"就会作为附加上下文注入。不用重启。

### 第 3 步：模拟测一下

不用真起 agent，直接模拟触发：

```bash
# 用验证脚本（不依赖 Rust）
bash scripts/hooks_test.sh test UserPromptSubmit --stdin '{"prompt":"你好"}'
# 将来：ion hooks test UserPromptSubmit --stdin '{"prompt":"你好"}'
```

输出会显示：stdin → handler 执行 → exit code → stdout → 结果（CONTINUE/BLOCK）的完整链路。

> `scripts/hooks_test.sh` 是项目自带的纯 bash 验证脚本（依赖 jq），模拟 `ion hooks test/validate/list` 的核心逻辑。在 `ion hooks` 内置命令实现前，用它验证你的配置。

---

## 3. 配置格式

### 3.1 配置文件位置

两个位置，**合并执行**（同事件的 hooks 拼接，不覆盖）：

| 文件 | 生效范围 | 适合放什么 |
|------|---------|-----------|
| `~/.ion/hooks.json` | 全局（所有项目） | 个人习惯、跨项目通用规则 |
| `<project>/.ion/hooks.json` | 仅当前项目 | 项目专属规则，可 git 追踪给团队共享 |

### 3.2 整体结构

```json
{
  "version": 1,
  "disableAllHooks": false,
  "hooks": {
    "<事件名>": [
      {
        "type": "<handler 类型>",
        "...": "handler 特有字段"
      }
    ]
  }
}
```

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `version` | number | 1 | 配置版本，当前仅支持 1 |
| `disableAllHooks` | bool | false | **紧急逃生阀**——设 true 全局禁用所有 hooks |
| `hooks` | object | — | 事件名 → handler 配置列表的映射 |

### 3.3 事件名（12 个）

| 事件 | 什么时候触发 | 能不能阻断 |
|------|------------|-----------|
| `SessionStart` | 会话创建后 | ❌ |
| `Setup` | 会话首次启动（仅 startup 时额外触发） | ❌ |
| `SessionEnd` | 会话关闭 | ❌ |
| `PreCompact` | 上下文压缩前 | ❌ |
| `UserPromptSubmit` | 用户提交输入后、agent 处理前 | ✅ 可 block |
| `PreToolUse` | 工具调用前 | ✅ 可 deny/ask/改参数 |
| `PostToolUse` | 工具调用成功后 | ✅ 可 block（通知 LLM） |
| `PostToolUseFailure` | 工具调用失败时 | ✅ 可 block |
| `PermissionRequest` | 权限请求时 | ✅ 可 deny |
| `SubagentStart` | 子 agent 启动 | ❌ |
| `SubagentStop` | 子 agent 结束 | ✅ block 后 reason 作为新 query |
| `Stop` | agent 准备停止 | ✅ block 后 reason 作为新 query |
| `Notification` | 通知场景（异步，不阻塞） | ❌ |

### 3.4 Handler 类型（5 种）

每个 handler 有一个 `type` 字段，决定"执行什么动作"：

#### `command` — 跑 shell 命令（最常用）

```json
{
  "type": "command",
  "command": "bash .ion/scripts/check.sh",
  "timeout": 5
}
```

**机制**：
- spawn 一个 `bash -c <command>` 子进程
- stdin 写入 JSON（事件上下文）
- stdout 读出来——如果是 JSON，解析控制指令；如果是纯文本，当作附加上下文
- **退出码决定结果**：
  - `exit 0` = 正常（stdout 注入或忽略）
  - `exit 2` = 阻断（stderr 作为错误原因）
  - `exit 3` = 请求用户确认（仅 PreToolUse）
  - 其他 = 非阻断错误，忽略

**stdin 里有什么**（command 脚本能读到）：

| 事件 | stdin 关键字段 |
|------|---------------|
| 所有 | `session_id`, `cwd`, `hook_event_name` |
| `PreToolUse`/`PostToolUse` | `tool_name`, `tool_input`, `tool_response`(PostToolUse) |
| `UserPromptSubmit` | `prompt`（用户输入的原文） |
| `Stop`/`SubagentStop` | `last_assistant_message`, `loop_count` |

**stdout 能控制什么**（command 脚本输出的 JSON）：

```json
{
  "decision": "block",
  "reason": "阻断原因",
  "hookSpecificOutput": {
    "additionalContext": "这段文字会注入到 system prompt",
    "permissionDecision": "allow",
    "updatedInput": {}
  }
}
```

**环境变量**（command 脚本能读）：
- `ION_HOOK_EVENT` — 事件名
- `CLAUDE_PROJECT_DIR` — 项目根目录（兼容 pi 脚本）

#### `http` — POST 到 URL

```json
{
  "type": "http",
  "url": "https://your-server.com/hook",
  "timeout": 10
}
```

**机制**：POST stdin JSON 到 url，按 HTTP 状态码解释（200 正常 / 403 阻断）。**强制 HTTPS，拒绝私网 IP**。

#### `prompt` — 单轮 LLM 判断（无工具）🔧 暂未实现

> ⚠️ **stub 状态**：当前 `run_prompt` 记录日志后返回默认值（不阻断）。需要 Extension trait 暴露 `call_llm` 能力后才能完整实现。**暂时用 `command` handler + 脚本调 LLM API 替代**。

```json
{
  "type": "prompt",
  "prompt": "判断用户输入是否合规。合规返回 {\"block\":false}，不合规返回 {\"block\":true,\"reason\":\"...\"}",
  "model": "fast",
  "timeout": 30
}
```

**机制**（实现后）：调一次 LLM（maxTokens 1024，**不给工具**），让它返回 JSON 决策。适合"简单判断"，不适合需要多轮工具调用的任务。

#### `agent` — 起一个带工具的子 Agent ⭐

```json
{
  "type": "agent",
  "agent": "outline-syncer",
  "prompt": "扫描 docs/ 下 MD，更新 outline.json。",
  "model": "fast",
  "max_turns": 100,
  "allowed_tools": ["read", "write", "edit", "bash", "grep", "find"],
  "timeout": 300
}
```

| 字段 | 必填 | 说明 |
|------|------|------|
| `agent` | 否 | 指定 agent 角色（`.ion/agents/<name>.md`）。不填用 `default` |
| `prompt` | 是 | 任务指令（注入 stdin 上下文后发给子 Worker） |
| `model` | 否 | 指定模型（tier alias 或 provider/id），不填用会话模型 |
| `allowed_tools` | 否 | 工具白名单，不填继承全部 |
| `max_turns` | 否 | 最大步数，默认 50 |

**机制**：spawn 一个子 Worker，带工具循环，能真正读文件、改文件、多轮操作。**这是 ION 比 pi 强的地方**——pi 的 agent handler 名义能调 agent，实际不传工具退化成单轮 LLM；ION 真传工具 + maxTurns。

**选哪个 agent**：
- 不填 `agent` → 用 `default`（通用 agent，有全部工具）
- 填 `"agent": "outline-syncer"` → 用 `.ion/agents/outline-syncer.md`（自定义角色，可定义 system prompt / 工具限制 / 专用模型）
- prompt 里写的指令会作为 task 发给子 Worker；agent.md 里定义的 system prompt 会作为它的角色设定

**适用场景**：需要"检查 + 更新"这类多步操作的重任务。

#### `mcp_tool` — 调 MCP server 的工具 🔧 暂未实现

> ⚠️ **stub 状态**：当前 `run_mcp_tool` 记录日志后返回默认值。需要 Extension trait 暴露 `McpManager` 能力后才能完整实现。**暂时用 `command` handler + 脚本调 MCP 工具替代**。

```json
{
  "type": "mcp_tool",
  "server": "knowledge-base",
  "tool": "kb_search",
  "input": {"query": "扩展系统"}
}
```

**机制**（实现后）：调配置好的 MCP server 的某个工具。pi 没实现这个（no-op），ION 补上。

### 3.5 Handler 通用字段

所有 handler 类型都支持的可选字段：

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `timeout` | number | 30 | 超时秒数 |
| `if` | string | — | 条件表达式，如 `Bash(rm *)` 表示只对 bash 删文件命令生效 |
| `async` | bool | false | 后台跑不阻塞（仅 PreToolUse） |
| `asyncRewake` | bool | false | async + exit 2 时把 reason 作为下一轮消息注入 |
| `once` | bool | false | 只触发一次 |
| `statusMessage` | string | — | 执行时给 UI 显示的状态文案 |
| `model` | string | — | prompt/agent 类型：指定模型（tier alias 或 provider/id） |
| `max_turns` | number | 50 | agent 类型：子 agent 最大步数 |
| `allowed_tools` | string[] | 全部 | agent 类型：子 agent 工具白名单 |

### 3.6 matcher（PreToolUse/PostToolUse 专用）

这两个事件可以用 matcher 过滤工具名：

```json
{
  "matcher": "bash|write|edit",
  "hooks": [
    { "type": "command", "command": "bash check.sh" }
  ]
}
```

- `matcher: "*"` 或不写 = 匹配所有工具
- `matcher: "bash|write"` = 只匹配 bash 和 write 工具
- 支持正则

---

## 4. 热重载（核心特性）

**策略：每次事件触发时动态读 hooks.json，不缓存。**

这意味着：

| 改了什么 | 生效方式 | 要额外操作吗 |
|---------|---------|------------|
| `hooks.json`（加/删/改 handler） | 下次事件触发读最新 | ❌ 不用 |
| handler 引用的 shell 脚本 | 每次 spawn bash 重跑，天然读最新 | ❌ 不用 |
| agent handler 引用的 agent.md | 子 Worker 每次创建时读 | ❌ 不用 |

**唯一边界**：正在跑的 handler 不打断——它用触发那一刻的配置跑完，下次触发才用新配置。

日常使用就是：**改完存盘即生效，不用重启，不用跑 reload 命令。**

---

## 5. CLI 命令速查

> 完整测试用例见 [HOOKS_CLI_TEST.md](./HOOKS_CLI_TEST.md)。这里只列用法。

### 5.1 `ion hooks` 子命令族

#### 校验 / 查看配置

```bash
ion hooks validate <path>     # 校验 hooks.json 格式，报错精确到字段
ion hooks list [--global|--project] [--json]   # 列出生效配置（全局+项目级合并）
ion hooks show <event>        # 看某事件的完整配置
```

#### 调试 handler（核心）

```bash
ion hooks test <event> [--stdin <json>] [--handler <n>]   # 模拟触发，真跑 handler
ion hooks dry-run <event> [--stdin <json>]                # 只看过滤不真跑
```

`ion hooks test` 是**最常用的调试命令**——不用真起 agent 就能测 handler 实际跑出什么。

#### 实时观察

```bash
ion hooks watch [--event <name>] [--session <sid>]   # 实时事件流
ion hooks trace <event> --last                       # 链路追踪（展开完整管道）
ion hooks stats [--since 1h]                         # 聚合统计
ion hooks log [--tail 20] [--failed]                 # 执行日志
```

#### 控制

```bash
ion hooks enable / disable [--session <sid>]   # 开关
ion hooks reload                                 # 手动重载（日常不用）
ion hooks init                                   # 生成模板配置 + 脚本
```

> **没有 `ion outline` 命令**——大纲同步是用户用 hooks 搭的用例（见 §7 附录），不是内置 CLI。查看大纲状态用你自己的脚本（如 `jq .ion/outline.json`）。

---

## 6. 数据链路（hook 在数据管道里的位置）

hook 不是孤立的。下图是一次 `SubagentStop` 触发的完整链路（以大纲同步为例，但链路结构适用于任何 agent handler 场景）——**理解这张图，调试时就知道问题出在哪段**：

```
┌─ 上游：事件源 ─────────────────────────────────────────────┐
│  子 agent 跑完                                            │
│    ↓ 产生 stdin JSON                                     │
│  {session_id, last_message, loop_count}                  │
└──────────────────────────────────────────────────────────┘
                         ↓
┌─ 中游：hook 引擎 ────────────────────────────────────────┐
│  读 hooks.json（每次都读，改完即生效）                     │
│    ↓                                                     │
│  matcher/if 过滤 → 决定哪些 handler 真正跑                │
│    ↓                                                     │
│  handler 执行：                                          │
│    agent handler                                         │
│      ├─ spawn 子 Worker（带工具 + maxTurns）              │
│      ├─ 子 Worker 跑工具循环（read MD → write outline）   │
│      └─ 退出 → exit code + 输出                          │
└──────────────────────────────────────────────────────────┘
                         ↓
┌─ 下游：结果影响 ─────────────────────────────────────────┐
│  按 exit code 分流：                                      │
│    exit 0 + stdout 文本 → 注入当前会话 system prompt      │
│    exit 2 (block) → 吞输入 / reason 作为新 query          │
│    exit 3 (ask) → 弹 UI 确认                              │
│  agent handler 额外副作用（由你的脚本/agent 决定）：       │
│    ├─ 子 Worker 改了业务文件（如 outline.json）            │
│    └─ emit 自定义事件 → 所有终端收到通知（事件名你自己定）│
└──────────────────────────────────────────────────────────┘
```

**每个 CLI 命令观察链路的哪段**：

| 想看哪段 | 用哪个命令 |
|---------|-----------|
| 配置加载对不对 | `validate` / `list` / `show` |
| handler 真跑出什么 | `test` / `dry-run` |
| 实时看每次触发 | `watch` |
| 一次执行的完整管道 | `trace` |
| 整体统计 | `stats` |
| 历史排查 | `log` |
| agent handler 改的业务数据 | 用你自己的脚本（如 `jq .ion/outline.json`） |

---

## 附录 B：从简单到高级 — 3 个实战教程

> 这 3 个示例递进：入门（拦截命令）→ 中级（注入上下文）→ 高级（多事件协作）。
> 全部是**用户自己搭的**，内核只提供 hooks 能力。

### B.1 入门：拦截 `git commit --no-verify`

**目标**：agent 想提交代码时，如果带了 `--no-verify`（跳过测试），直接拦截。

**为什么这个最简单**：只用 1 个事件（PreToolUse）+ 1 个 command handler + exit 2 阻断。不用写 agent，不用注入上下文。

**配置**（`.ion/hooks.json`）：

```json
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash .ion/scripts/block_no_verify.sh",
            "timeout": 5,
            "statusMessage": "检查 git --no-verify..."
          }
        ]
      }
    ]
  }
}
```

**脚本**（`.ion/scripts/block_no_verify.sh`）：

```bash
#!/bin/bash
set -euo pipefail

# stdin 是 hook context JSON（包含 tool_input）
INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

# 检测 --no-verify
if echo "$COMMAND" | grep -qi "git.*--no-verify"; then
    echo '{"decision":"block","reason":"禁止使用 --no-verify，测试必须跑"}'
    exit 2
fi

exit 0
```

**怎么测**：

```bash
bash scripts/hooks_test.sh validate .ion/hooks.json
bash scripts/hooks_test.sh test PreToolUse --stdin '{"tool_name":"bash","tool_input":{"command":"git commit --no-verify -m test"}}'
# → 应显示 block: true, reason: "禁止使用 --no-verify"
```

**学到了什么**：
- PreToolUse + matcher 拦截特定工具
- exit 2 + JSON decision 阻断
- stdin 里能拿到 tool_input（agent 要执行的命令）

---

### B.2 中级：每次提问注入项目约定

**目标**：用户每次提问时，自动把项目的代码约定（从 `.ion/conventions.md` 读）注入到 agent 的上下文，不用每次手动说。

**为什么这是中级**：用 UserPromptSubmit + command handler 的 stdout 注入。不阻断，只追加。脚本要读文件。

**配置**：

```json
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "bash .ion/scripts/inject_conventions.sh",
        "timeout": 3
      }
    ]
  }
}
```

**脚本**（`.ion/scripts/inject_conventions.sh`）：

```bash
#!/bin/bash
set -euo pipefail

CONV=".ion/conventions.md"
[ ! -f "$CONV" ] && exit 0

# stdout 直接作为 additionalContext 注入（纯文本模式）
echo "=== 项目代码约定 ==="
cat "$CONV"
```

**约定文件**（`.ion/conventions.md`）：

```markdown
- 用 Rust 写代码，不用 C
- 错误处理用 anyhow，不用 unwrap
- 每个公开函数必须有文档注释
```

**怎么测**：

```bash
bash scripts/hooks_test.sh test UserPromptSubmit --stdin '{"prompt":"帮我写个函数"}'
# → stdout 显示项目约定，会被注入到 system prompt
```

**效果**：agent 每次收到用户输入时，上下文里都自动带了"用 Rust / 用 anyhow / 要文档注释"，不需要用户每次提醒。

**学到了什么**：
- UserPromptSubmit 的 stdout 纯文本 = additionalContext（自动注入）
- 不 return exit 2 = 不阻断，只是追加
- 脚本能读项目文件

---

### B.3 高级：Stop 强制检查测试

**目标**：agent 觉得干完了想停下时，强制跑一次测试。如果测试没过，不让停——把"测试失败，请修复"作为新指令注入，让 agent 继续。

**为什么这是高级**：用 Stop 事件 + exit 2 阻断 + reason 自动作为新 query。还有 loop_limit 防死循环。多个概念组合。

**配置**：

```json
{
  "version": 1,
  "hooks": {
    "Stop": [
      {
        "loop_limit": 3,
        "hooks": [
          {
            "type": "command",
            "command": "bash .ion/scripts/check_tests.sh",
            "timeout": 60,
            "statusMessage": "运行测试..."
          }
        ]
      }
    ]
  }
}
```

**脚本**（`.ion/scripts/check_tests.sh`）：

```bash
#!/bin/bash
set -euo pipefail

# 跑测试（cargo test / npm test / pytest，按项目情况改）
OUTPUT=$(cargo test 2>&1) || true

if echo "$OUTPUT" | grep -q "test result: FAILED\|FAILED\|error\["; then
    # 测试失败 → exit 2，reason 作为新 query 让 agent 继续修
    echo '{"decision":"block","reason":"测试失败，请修复后再次运行测试确认通过：\n'"$(echo "$OUTPUT" | tail -20 | sed 's/"/\\"/g' | tr '\n' ' ')"'"}'
    exit 2
fi

exit 0
```

**工作流**：

```
agent 干完活 → 想停下（Stop）
    ↓
check_tests.sh 跑测试
    ├─ 通过 → exit 0 → agent 正常停
    └─ 失败 → exit 2 + "测试失败，请修复..."
        ↓ reason 作为新 query 注入
        agent 收到"测试失败，请修复" → 继续干活
        ↓
        loop_count=1，再跑到 Stop → 再测
        ↓ loop_limit=3
        最多重试 3 次，第 4 次强制允许停（防死循环）
```

**怎么测**：

```bash
bash scripts/hooks_test.sh test Stop --stdin '{"last_assistant_message":"我做完了"}'
# → 如果测试没过，应显示 block + reason
```

**学到了什么**：
- Stop 事件 block 后 reason 自动作为新 query（内核通过 follow_up_tx 注入）
- loop_limit 防死循环（默认 5，这里设 3）
- handler 能跑耗时操作（timeout 60s）

---

### B.4 教程递进总结

| 级别 | 示例 | 事件 | 核心机制 | 能力要点 |
|------|------|------|---------|---------|
| **入门** | 拦截 --no-verify | PreToolUse | exit 2 阻断 | matcher 过滤 + stdin 拿 tool_input |
| **中级** | 注入项目约定 | UserPromptSubmit | stdout 纯文本注入 | 读文件 + additionalContext |
| **高级** | 强制检查测试 | Stop | exit 2 + reason 新 query + loop_limit | 阻断 + 续命 + 防死循环 |
| **完整** | 大纲同步（附录 A） | UserPromptSubmit + SubagentStop | command 注入 + agent handler | 多事件协作 + 后台 agent |

掌握了入门→中级→高级，再看附录 A 的大纲同步就很自然了——它只是"中级（注入）+ 高级（多事件）"的组合。

---

## 附录 A：完整示例 — MD ↔ Outline 大纲同步

> ⚠️ **这是一个示例**，展示如何用 hooks 能力搭出一个完整的业务功能。
> **不是内置能力**——内核不提供 `ion outline` 命令、不内化 outline 格式、不硬编码同步逻辑。
> 你可以直接复制下面的配置和脚本，或参照它搭你自己的场景（文档审查、代码扫描、配置同步……）。

### A.1 场景

项目有大量设计文档（`docs/**/*.md`），你想：
1. 每次用户提问时，把文档大纲注入上下文（让 agent 知道有哪些文档）
2. 子 agent 干完活后，自动检测文档变更，更新大纲索引

### A.2 配置

**文件**：`<project>/.ion/hooks.json`

```json
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "bash .ion/scripts/inject_outline.sh",
        "timeout": 3,
        "statusMessage": "注入文档大纲..."
      }
    ],
    "SubagentStop": [
      {
        "type": "agent",
        "agent": "outline-syncer",
        "prompt": "扫描 docs/ 下所有 .md，对比 .ion/outline.json 的 mtime 和 hash，找出不同步的并更新 outline.json。",
        "model": "fast",
        "max_turns": 100,
        "allowed_tools": ["read", "write", "edit", "bash", "grep", "find"],
        "timeout": 300,
        "async": true
      }
    ]
  }
}
```

### A.3 配套脚本

**文件**：`<project>/.ion/scripts/inject_outline.sh`

```bash
#!/bin/bash
set -euo pipefail
OUTLINE=".ion/outline.json"
[ ! -f "$OUTLINE" ] && exit 0
jq -r '
  "=== 项目文档大纲 ===\n",
  .entries[] |
  "## \(.title)\n  路径: \(.path)\n  摘要: \(.summary)\n"
' "$OUTLINE" 2>/dev/null || exit 0
```

### A.4 大纲格式

**文件**：`<project>/.ion/outline.json`

```json
{
  "version": 1,
  "generated_at": "2026-07-13T10:00:00Z",
  "scan_root": "docs/",
  "entries": [
    {
      "path": "docs/design/EXTENSION_SYSTEM.md",
      "title": "WASM 扩展系统",
      "mtime": "2026-07-12T15:30:00Z",
      "content_hash": "a1b2c3d4",
      "last_synced": "2026-07-13T10:00:00Z",
      "summary": "WASM 扩展系统：热更新、4 维数据存储、16 个宿主函数",
      "headings": ["## 概览", "## 生命周期"]
    }
  ]
}
```

### A.5 怎么用

```bash
# 1. 建配置和脚本（手动，或参照 ion hooks init 生成的模板改）
mkdir -p .ion/scripts
# 把上面的 hooks.json 和 inject_outline.sh 放好

# 2. 校验配置
bash scripts/hooks_test.sh validate .ion/hooks.json

# 3. 测注入脚本（不用真起 agent）
bash scripts/hooks_test.sh test UserPromptSubmit --stdin '{"prompt":"什么是扩展系统"}'

# 4. 看大纲状态（用你自己的脚本/jq，不是内置命令）
jq '.entries | length' .ion/outline.json   # 几个文档
jq '.entries[].path' .ion/outline.json     # 列出路径

# 5. 触发一次同步（正常由 SubagentStop 自动触发；手动验证用 hooks test）
bash scripts/hooks_test.sh test SubagentStop --stdin '{"last_message":"done"}'
```

---

## 8. 常见问题

### Q: 改了 hooks.json 为什么没生效？
A: 检查三件事：
1. `ion hooks validate <path>` 格式对不对
2. `ion hooks list` 有没有显示你的 handler
3. `disableAllHooks` 是不是被设成了 true

### Q: handler 跑了但结果不对？
A: 用 `ion hooks test <event> --stdin '{...}'` 模拟触发，看 handler 的 stdout 到底是什么。用 `ion hooks trace <event> --last` 看最近一次的完整管道。

### Q: agent handler 太慢了？
A: 用 `ion hooks stats` 看平均耗时。如果确实慢，考虑：
- 减小 `allowed_tools`（减少工具上下文）
- 减小 `max_turns`
- 换更快的 `model`（如 `fast`）
- 设 `async: true` 让它后台跑不阻塞

### Q: 怎么临时关掉所有 hooks？
A: `ion hooks disable`（当前会话）或在 hooks.json 里设 `"disableAllHooks": true`（全局）。

### Q: agent handler 和 command handler 怎么选？
A:
- 简单校验/注入文本/跑个脚本 → `command`（快，几十 ms）
- 需要多轮工具调用（读文件、改文件、搜索）→ `agent`（慢，几秒到几分钟，但能干重活）
- 单次 LLM 判断（合规检查）→ `prompt`（中等，单轮无工具）

---

## 9. 对标 pi

本系统对齐 pi 的 `extensions/pi-hooks/`，但有三个差异：

| 对比项 | pi | ION |
|--------|-----|-----|
| agent handler | 名义能调 agent，实际不传 tools，退化成单轮 LLM | **真传 tools + maxTurns**，能干重活 |
| mcp_tool handler | no-op（没实现） | 真实现，能调 MCP 工具 |
| 事件数 | 12 个 | 12 个（对齐） |
| 配置热重载 | 文件签名检测 + 缓存 | **每次触发动态读**（更简单，零延迟） |

详见实现文档 [HOOKS_AND_OUTLINE_SYNC.md §1 背景与对标分析](./HOOKS_AND_OUTLINE_SYNC.md)。
