# improver — 通用任务智能体

> **状态：已实现，待 e2e 验证** — builtin 注册完成，调研类/改代码类流程待真实跑通。

## 一句话

用户给一个话题（修 bug / 加功能 / 重构 / 调研），improver 自己完成全流程：开 worktree → 改代码 → container 编译测试 → commit → 导出 HTML 报告 → 反馈给调用方。

## 核心架构：外面是主，里面是执行环境

```
ZCode (用户) → ion --agent improver "话题"
                 │
                 │ host (improver) ← 智能在这里
                 │   ├─ git worktree add（代码隔离）
                 │   ├─ read/edit 改代码（自己动手）
                 │   ├─ container run（挂载 worktree，启动隔离环境）
                 │   ├─ container exec cargo build（把活派给容器跑）
                 │   ├─ container exec cargo test（拿结果回来判断）
                 │   ├─ 失败就继续改 + 重试（≤3 次）
                 │   ├─ git commit + ion --export
                 │   └─ 反馈报告给 ZCode
                 │
                 │ container（纯执行环境）← 无脑跑
                 │   ├─ 只有 rustc/cargo/git（image 预装）
                 │   ├─ worktree 挂载在 /workspace（实时同步）
                 │   └─ 收到 container exec 命令 → 跑 → 输出 → 退
```

**核心原则**：

| 原则 | 说明 |
|------|------|
| **外面是主** | improver 在 host 上改代码、判断测试结果、决策流程。智能全在 host |
| **里面是执行环境** | container 只是带 Rust 工具链的隔离编译/测试沙箱。**不装 ion**，不跑 agent |
| **worktree 挂载** | host 改的代码实时同步到 container，不需要 git push/pull |
| **不降级** | container 不可用就报错退出，不 fallback 到 host 直接 cargo |

## 4 类话题分流

improver 收到话题后先分类：

| 类型 | 触发词 | 流程 |
|------|--------|------|
| **改代码类** | 修复 / 增加 / 添加 / 重构 / 优化 / 改写 | 完整流程（9 步，含 container） |
| **调研类** | 分析 / 理解 / 评估 / 看看 / 检查 | 调研流程（4 步，不开 container，不改代码） |

不确定时默认按"改代码类"走（更严格）。

## 完整流程（改代码类，9 步）

| Step | 动作 | 工具 | 失败处理 |
|------|------|------|---------|
| 1 | `git worktree add` 开隔离 | bash | 失败退出 |
| 2 | `bash scripts/init-evolve-container.sh` 起 container | bash | **失败退出（不降级）** |
| 3 | read 相关文件 + edit 改代码 | read/edit | — |
| 4 | `container exec cargo build --bin ion` 编译验证 | bash | 失败重试 ≤3 次 |
| 5 | `container exec cargo test --lib` 测试验证 | bash | 失败重试 ≤3 次 |
| 6 | `git add -A && git commit` | bash | — |
| 7 | `ion --export /tmp/improver_*.html` 导出报告 | bash | — |
| 8 | `container stop` + `git worktree remove` 清理 | bash | — |
| 9 | 输出总结反馈 | — | — |

## 调研流程（4 步，不改代码）

| Step | 动作 |
|------|------|
| 1 | 开 worktree（隔离） |
| 2 | read/grep/find 分析代码 |
| 3 | 输出分析报告（文本，不 commit） |
| 4 | 导出 HTML |

## 关键实现点

### 1. improver 是 builtin agent（不是用户 .md）

在 `src/agent_config.rs` 的 `builtin_agents()` 里注册，prompt 用 `include_str!("../examples/agents/improver.md")` 编译期嵌入：

```rust
const IMPROVER_MD: &str = include_str!("../examples/agents/improver.md");

pub fn builtin_agents() -> Vec<AgentConfig> {
    let improver = parse_agent_md(IMPROVER_MD, "examples/agents/improver.md")
        .unwrap_or_else(|| /* 最小兜底配置 */);
    vec![build_builtin(), explore_builtin(), plan_builtin(), improver]
}
```

**好处**：
- `ion --agent improver` 开箱即用，无需 cp 到 `~/.ion/agent/agents/`
- 改 `examples/agents/improver.md` 后 `cargo build` 即生效
- 命中 `find_agent` 的搜索路径 #4（builtin fallback）

### 2. bash timeout 通过环境变量配置

| 环境变量 | 作用 | 默认值 | 用途 |
|---------|------|--------|------|
| `ION_BASH_TIMEOUT` | BashTool 底层 timeout（秒） | 180 | `src/agent/tool.rs` BashTool |
| `ION_TOOL_TIMEOUT` | agent_loop 外层 timeout（秒） | 600 | `src/agent/agent_loop.rs` 对 bash/bash_run/skill 的包装层 |

improver 跑改代码类任务时建议：
```bash
ION_TOOL_TIMEOUT=1800 ION_BASH_TIMEOUT=1800 ion --agent improver "..."
# 30 分钟，够 container 里 cargo build + test
```

不设就跟原来一样（180s / 600s），不影响其他 agent。

### 3. container 通过 init-evolve-container.sh 启动

improver 的 prompt Step 2 直接调 `scripts/init-evolve-container.sh "$WT_DIR"`。脚本做的事：
- 检查 container 系统 + 镜像（`ion-evolve-rust:latest`，不存在则从 `Dockerfile.evolve` 构建）
- `container run -v $WT_DIR:/workspace --memory 4G --cpus 4`
- 修复 Cargo.toml 路径（`../ion-provider` → `/ion-provider`）
- 输出 `CONTAINER_NAME=ion-evolve-XXX`

### 4. worktree 挂载实现实时同步

`-v $WT_DIR:/workspace` 让 host 的 worktree 目录映射到 container 的 `/workspace`。
host 用 edit 改 `$WT_DIR/src/xxx.rs`，container 里 `/workspace/src/xxx.rs` 立刻可见。
不需要 git push/pull，不需要 patch 文件。

## 跟 evolver 的区别

| 维度 | evolver | improver |
|------|---------|----------|
| 定位 | 自我进化专用（ION 改 ION 自己） | 通用任务（任何项目） |
| 发现方式 | `examples/agents/evolver.md`（需 cp） | builtin（开箱即用） |
| 话题范围 | 只改 ION 自己 | 修 bug / 加功能 / 重构 / 调研 |
| container | prompt 里没落地（设计文档有，代码没） | prompt 里明确 Step 2 调脚本 |
| 不降级 | 未明确 | 明确：container 不可用就退出 |
| 调研模式 | 没有 | 有（不改代码，不开 container） |

evolver 作为"自我进化专用版"保留，improver 是它的通用化升级。

## 失败处理

| 场景 | 处理 |
|------|------|
| container CLI 不可用 | 报错退出，提示前置条件 |
| 镜像没构建 | `init-evolve-container.sh` 自动从 Dockerfile.evolve 构建 |
| 编译失败 | read 错误 → edit 修复 → 重试（≤3 次） |
| 测试失败 | read 错误 → edit 修复 → 重试（≤3 次） |
| 3 次重试都失败 | 跳到 Step 9 输出失败报告（含 HTML） |
| container 编译 OOM | `init-evolve-container.sh` 已限 `--memory 4G`，单 binary 不 OOM |

## 验证方式

```bash
# 1. 确认 builtin 注册
ion list-agents | grep improver

# 2. 调研类（不开 container，快）
ion --agent improver "分析 src/global_memory.rs 的搜索逻辑，找出潜在问题"

# 3. 改代码类（开 container，完整流程）
ION_TOOL_TIMEOUT=1800 ion --agent improver "给 global_memory 加 fn clear_all() 方法"

# 4. container 不可用（报错退出）
PATH=/usr/bin:/bin ion --agent improver "修个 bug"
```

## 相关文件

| 文件 | 作用 |
|------|------|
| `examples/agents/improver.md` | agent 定义（prompt + frontmatter） |
| `src/agent_config.rs` | builtin 注册（`include_str!` 嵌入） |
| `src/agent/agent_loop.rs` | `ION_TOOL_TIMEOUT` 环境变量 |
| `src/agent/tool.rs` | `ION_BASH_TIMEOUT` 环境变量 |
| `scripts/init-evolve-container.sh` | container 启动脚本 |
| `scripts/Dockerfile.evolve` | container 镜像定义 |
