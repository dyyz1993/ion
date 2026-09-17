---
name: create-subagent
description: 如何在 ION 中创建子智能体（agent .md 定义 + spawn_worker 派发）——含定义生效时机与编排模式
---

# 创建子智能体

ION 的子智能体 = 一个 `.md` 定义文件（系统提示词+工具白/黑名单+参数）+ `spawn_worker` 工具派发。host 上的 Manager 管生命周期，父 agent 通过 await/channel 与子 agent 协作。

## Agent .md 定义位置与优先级

| 位置 | 优先级 | 内置例子 |
|---|---|---|
| `<project>/.ion/agents/*.md` | 最高（覆盖同名） | — |
| `~/.ion/agent/agents/*.md` | 中 | reviewer.md 等 |
| 内置（编译进二进制） | 最低 | build / coordinator / developer / reviewer / qa 等 |

frontmatter 字段：`name`、`description`、`system_prompt`（或正文即提示词）、`tools`（白名单）/`disallowed_tools`（黑名单）、`max_turns`。

## 生效时机（重要）

| 动作 | 何时生效 |
|---|---|
| 创建 agent .md 文件 | **下一个新会话/新 worker 启动时生效**（agents 列表在会话启动时扫描注入 system prompt 的 available-agents 大纲）。当前正在运行的会话看不到新文件 |
| `spawn_worker` 工具本身 | **当前会话立即可用**（内核内置工具，无需任何配置）——但引用自定义 agent 名要等新会话（见上一行）。spawn 内置 agent 名（build/developer/reviewer…）当前会话直接可用 |
| 修改 agent .md | 只影响之后 spawn 的子 agent（spawn 时读取定义）；已运行的子 agent 不受影响 |
| spawn_worker 参数 max_turns | 当前会话即时生效（每次 spawn 单独传） |

## 两种用法

**A. 快速派子任务（无需任何文件，当前会话即可用）：**
```
spawn_worker(agent="build", initial_prompt="去 /tmp 分析日志文件并汇总错误",
             wait=true)   # wait=true 同步等结果；false 异步，完成时收到通知
```

**B. 自定义专属 agent（需新会话生效）：**
1. 写 `~/.ion/agent/agents/my-analyst.md`：
```markdown
---
name: my-analyst
description: 专职数据分析
disallowed_tools: ["bash", "write_file"]
max_turns: 50
---
你是数据分析专员。只读数据、给结论，不修改任何文件。
```
2. 新开会话后：`spawn_worker(agent="my-analyst", initial_prompt="...", wait=false)`

## 编排模式

- **同步子任务**：`wait=true`，父 agent 阻塞等结果（适合拆步骤的确定性流程）
- **异步任务**：`wait=false`，子 agent 完成时自动给父发 follow_up 通知（适合监控/长任务）
- **过程通信**：子 agent 运行中可用 `channel_send` 群聊频道汇报进度
- **远程执行**：`spawn_worker(host="win38-a", ...)` 把子 agent 派到远程沙盒跑（代码在远端执行，会话回流本地）

## 常见坑

- 新写的 agent .md 当前会话 spawn 报 not found → 正常，下个会话才被扫描
- 子 agent 默认继承父的模型；可用 `model="zai/glm-5.2"` 单独指定
- 子 agent 的 turn 预算默认 20；复杂任务记得传 `max_turns=200`
- `wait=true` 会占住父 agent 一轮；长任务一律 `wait=false` + 通知
