---
name: create-hooks
description: 如何在 ION 中创建 Hooks（事件触发的自动化：命令/HTTP/提示词/agent/MCP 五种 handler）——含各配置的生效时机
---

# 创建 Hooks

Hooks = 会话生命周期事件（PreToolUse / PostToolUse / UserPromptSubmit / SessionStart / Stop 等）触发的动作。支持 5 种 handler：`command`（shell）/ `http`（POST）/ `prompt`（注入提示词）/ `agent`（起子 agent 处理）/ `mcp_tool`（调 MCP 工具）。

## 配置文件位置与优先级

| 文件 | 作用域 | 默认是否加载 |
|---|---|---|
| `~/.ion/hooks.json` | 全局（所有会话） | ✅ 是 |
| `<project>/.ion/hooks.json` | 项目级 | ⚠️ **默认不加载**（供应链信任门）——需在 `~/.ion/config.json` 的 `runtime.hooks_trust.trusted_projects` 加入项目路径才生效 |

## 生效时机（重要）

| 动作 | 何时生效 |
|---|---|
| 创建/修改 `~/.ion/hooks.json`（全局） | **下一个新会话/新 worker 启动时生效**（HookExtension 在会话启动时一次性读取配置）。**当前正在运行的会话不会热加载**——改完需新开会话，或用 follow_up/steer 在新轮里自己执行等价逻辑 |
| 创建项目级 `.ion/hooks.json` | 需两步都完成才生效：① 加入 trusted_projects ② 新会话启动。当前会话内创建的文件即使已信任也不会热加载 |
| 修改 handler 脚本本体（hooks.json 指向的 .sh/.py） | **下一次触发即时生效**（每次事件触发时执行文件内容）——脚本改动无需重启会话 |

## hooks.json 格式（最小示例）

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash(rm *)",
        "handler": { "type": "command", "command": "echo '危险命令，需确认' >&2; exit 2" }
      }
    ],
    "PostToolUse": [
      {
        "matcher": "edit",
        "handler": { "type": "prompt", "prompt": "刚改过文件，检查是否需要跑测试" }
      }
    ]
  }
}
```

- `matcher`：事件过滤（工具名 / `Bash(rm *)` 条件表达式 / `*` 全匹配）
- handler exit code 语义：`0` 放行；`2` 拒绝（stderr 内容回给 LLM）；其他 = 非阻塞失败
- `command` handler 用 `stdin` 收事件 JSON（工具名/参数/会话信息）

## 创建步骤（推荐流程）

1. 写 handler 脚本（如 `~/.ion/hooks/my-hook.sh`）并 `chmod +x`
2. 手动用样例事件 JSON 测一遍：`echo '{"tool":"Bash","params":{...}}' | bash my-hook.sh`
3. 写入 hooks.json（全局或项目级）
4. **新开会话验证**（当前会话测不出——见生效时机表）
5. 验证口：`ion rpc --method get_hooks --params '{}'`（列出已加载 hooks 及其 handler）

## 常见坑

- 项目级 hooks 不生效 → 90% 是没加 trusted_projects（安全默认）
- 改了 hooks.json 没反应 → 当前会话不热加载，新会话才读
- handler 里别做长阻塞（同步 handler 会卡住 agent 循环；长活用 `&` 后台化）
