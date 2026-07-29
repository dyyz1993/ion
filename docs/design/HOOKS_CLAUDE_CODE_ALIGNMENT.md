# ION Hooks 对齐 Claude Code — 设计文档

> **状态：已完成** — P1（4 bug）+ P2（glob matcher + 并行 handler + PostCompact/PermissionRequest 事件）全部修复。

## 一、事件覆盖（15/30）

### ION 支持的事件

| 事件 | Extension trait 钩子 | stdin_builder | 挂载位置 |
|------|---------------------|---------------|---------|
| SessionStart | on_session_start | ✅ source/reason | extension.rs |
| SessionEnd | on_session_shutdown | ✅ | extension.rs |
| PreCompact | on_session_before_compact | ✅ trigger/custom_instructions | extension.rs |
| **PostCompact** | on_session_compact | ✅ common_fields | extension.rs (新增) |
| UserPromptSubmit | on_input | ✅ prompt | extension.rs |
| PreToolUse | before_tool_call | ✅ tool_name/tool_input/tool_use_id | extension.rs |
| PostToolUse | after_tool_call | ✅ tool_response/tool_use_id | extension.rs |
| PostToolUseFailure | after_tool_call (is_error) | ✅ | extension.rs |
| **PermissionRequest** | on_permission_request | ✅ tool/args | extension.rs (新增) |
| SubagentStart | on_agent_start | ✅ | extension.rs |
| SubagentStop | on_agent_end | ✅ last_assistant_message | extension.rs |
| Stop | (agent_loop 内联) | ✅ last_assistant_message/stop_hook_active | extension.rs |
| Notification | (stdin_builder 有，未挂载) | ✅ | 后续 |
| Setup | on_session_start (startup) | ✅ | 后续 |
| PermissionDenied | (未实现) | — | 后续 |

### 未实现的 CC 事件（15 个，需新增 trait 钩子）

`StopFailure` / `InstructionsLoaded` / `MessageDisplay` / `TaskCreated` / `TaskCompleted` / `TeammateIdle` / `ConfigChange` / `CwdChanged` / `FileChanged` / `WorktreeCreate` / `WorktreeRemove` / `Elicitation` / `ElicitationResult` / `UserPromptExpansion` / `PostToolBatch`

## 二、Handler 类型（5/5）

| 类型 | 说明 | 实现位置 |
|------|------|---------|
| command | spawn bash + exit code 协议 | handler_runner.rs |
| http | POST URL + HTTPS 校验 + 私有 IP 阻止 | handler_runner.rs |
| prompt | 调 LLM 判断 | handler_runner.rs |
| agent | spawn worker（ION 原创超集） | handler_runner.rs |
| mcp_tool | MCP 工具调用 | handler_runner.rs |

## 三、stdin JSON 字段

### 通用字段（所有事件）

| 字段 | 来源 |
|------|------|
| session_id | ION_SESSION_ID env |
| cwd | current_dir() |
| transcript_path | session JSONL 路径 |
| hook_event_name | 事件名 |
| workspace_roots | [cwd] |
| permission_mode | ION_SECURITY_MODE env（可选）|

### 事件特有字段

| 事件 | 字段 |
|------|------|
| SessionStart | source, reason |
| PreCompact | message_count, trigger, custom_instructions |
| UserPromptSubmit | prompt |
| PreToolUse | tool_name, llm_tool_name, tool_input, tool_use_id |
| PostToolUse | tool_name, llm_tool_name, tool_input, tool_response, tool_use_id |
| Stop | last_assistant_message, stop_hook_active, loop_count |
| SubagentStop | last_assistant_message, stop_hook_active, loop_count |
| PermissionRequest | tool, args |

## 四、Exit Code 协议

| Exit Code | 含义 | 行为 |
|-----------|------|------|
| 0 | 成功 | 解析 stdout JSON |
| 2 | 阻断 | **按事件区分**：SessionStart/SessionEnd/Setup/SubagentStart/Notification 非阻断；其他阻断 |
| 3 | 请求确认 | ask（ION 独有，仅 PreToolUse）|
| 其他 | 非阻断错误 | 忽略 |

## 五、stdout JSON 输出

| 字段 | 支持 |
|------|------|
| decision: "block" | ✅ |
| reason | ✅ |
| hookSpecificOutput.additionalContext | ✅ |
| hookSpecificOutput.permissionDecision (allow/deny/ask/defer) | ✅ 全部支持 |
| hookSpecificOutput.updatedInput | ✅ 解析 + **应用到 call.arguments** |
| 顶层 additionalContext（简写）| ✅ |

## 六、Matcher

| 特性 | 实现 |
|------|------|
| 通配 `*` / 空 | 全匹配 |
| `\|` 分割精确匹配 | 大小写不敏感 |
| 单工具名精确匹配 | 大小写不敏感 |
| glob 模式（mcp__* / Bash*）| ✅ 复用 rules_engine glob_match |

## 七、执行模式

- **并行**：所有匹配的 handler 用 tokio::spawn 并行执行（对齐 Claude Code）
- **结果合并**：任一 block 则整体 block
- **once 去重**：同一 handler 每 session 只触发一次（ION 独有）
- **递归防护**：ION_HOOK_DEPTH 防 agent handler 死循环（ION 独有）

## 八、ION 超集（CC 没有的）

1. exit 3 → "ask" 语义
2. `once: true` 每 session 去重
3. `ION_HOOK_DEPTH` 跨进程递归防护
4. 每组 `loop_limit`（可配置，默认 5）
5. 硬编码私有 IP 阻止
6. `statusMessage` 预执行状态
7. `emit_handler_executed` 可观测性事件
8. 基于文件的 Agent 角色（`.ion/agents/*.md`）

## 九、测试覆盖

| 层 | 数量 |
|----|------|
| stdin_builder 单元测试 | 7 |
| hooks 模块单元测试 | 62 |
| hooks CI 脚本 | 5（hooks_ci / hooks_agent_ci / hooks_handler_ci / hooks_agent_real / hooks_stdin_ci）|
| hooks_e2e 集成测试 | 10 |
| **总计** | **84 个测试 + 5 个 CI 脚本** |

## 十、CI 验证

```bash
# 完整 hooks 验证
bash tests/hooks_ci.sh              # 事件加载 + command handler
bash tests/hooks_agent_ci.sh        # agent handler + 递归防护
bash tests/hooks_handler_ci.sh      # 5 种 handler 类型
bash tests/hooks_stdin_ci.sh        # stdin 字段完整性
bash tests/rules_ci.sh              # rules engine（独立但相关）

# Rust 测试
cargo test --lib hooks              # 69 个单元测试
cargo test --lib                    # 945 全量
```
