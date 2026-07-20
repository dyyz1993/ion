# improver — 通用任务智能体（host 改代码 + container 跑验证）

## 核心架构

**一个智能体、两个空间、明确分工**：

```
ZCode → ion --agent improver "话题"
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
- **外面是主，里面是执行环境**。improver 在 host 改代码、判断结果、决策流程
- **container 不装 ion**。它只是带 Rust 工具链的隔离编译/测试沙箱
- **worktree 挂载**让 host 改的代码实时同步到 container，不需要 git push/pull

## 改动清单（5 个文件）

### 1. 新建 `examples/agents/improver.md`（agent 定义）

frontmatter:
```yaml
---
name: improver
description: 通用任务智能体 — 给话题就能自己干（修bug/加功能/重构/调研）
tools:
  - read
  - bash
  - bash_run
  - ls
  - grep
  - find
  - edit
  - write
thinking_level: high
color: cyan
---
```

prompt 核心（含 4 类话题分流 + 完整 bash 模板）：

```
你是 ION 的通用任务智能体。用户给你一个话题，你自主完成全流程。

## 第零步：话题分类
判断话题属于哪类：
- 改代码类（修 bug / 加功能 / 重构）→ 走"完整流程"
- 调研类（分析 / 理解 / 评估，不改代码）→ 走"调研流程"

## 完整流程（改代码类）

### Step 1: 开 worktree（第一个 bash，必须执行）
WT_DIR=$(mktemp -d /tmp/ion-improver-XXXXXX)
git worktree add "$WT_DIR" -b "improve/$(date +%Y%m%d-%H%M%S)"
echo "WT_DIR=$WT_DIR"
# 后续所有 read/edit 都在 $WT_DIR 里

### Step 2: 起 container（挂载 worktree）
bash scripts/init-evolve-container.sh "$WT_DIR"
# 读输出 CONTAINER_NAME=ion-evolve-XXX
# 如果失败（container 不可用）→ 报错退出，不降级

### Step 3: 读代码 + 改代码（在 worktree 里）
cd "$WT_DIR"
read 相关文件 → 分析问题 → edit/write 改代码

### Step 4: container 里编译验证
CONTAINER=<上一步拿到的名字>
container exec $CONTAINER sh -c 'cd /workspace && cargo build --bin ion 2>&1 | tail -10'
# 失败 → read 错误 → edit 修复 → 重试（最多 3 次）

### Step 5: container 里测试验证
container exec $CONTAINER sh -c 'cd /workspace && cargo test --lib 2>&1 | tail -10'
# 失败 → 修复 → 重试（最多 3 次）

### Step 6: 提交
cd "$WT_DIR" && git add -A && git commit -m "improve: <一句话描述>"

### Step 7: 导出 HTML 报告
LAST_SID=$(cat ~/.ion/agent/last_session 2>/dev/null)
ion --export "/tmp/improver_$(date +%Y%m%d_%H%M%S).html" --session "$LAST_SID"

### Step 8: 清理
container stop $CONTAINER 2>/dev/null
cd /Users/xuyingzhou/Project/study-rust/ion  # 回主仓库
git worktree remove "$WT_DIR" --force 2>/dev/null

### Step 9: 反馈给 ZCode
✅ 任务完成
话题：<用户给的话题>
改动：<git diff --stat 的输出>
编译：✅
测试：✅ <N> passed
报告：/tmp/improver_xxx.html

## 调研流程（不改代码类）

### Step 1: 开 worktree（隔离，不污染主分支）
### Step 2: read + grep + find 分析代码（在 worktree 里）
### Step 3: 输出分析报告（文本，不 commit，不开 container）
### Step 4: 导出 HTML

## 铁律（违反 = 失败）
1. 改代码类第一个 bash 必须 git worktree add
2. container 不可用 → 报错退出（不降级到 host 直接跑）
3. 改代码必须在 worktree 里（不允许直接改主仓库）
4. 编译/测试必须在 container 里跑（不在 host 上直接 cargo）
5. 必须 git commit（改代码类）
6. 必须导出 HTML 报告
7. 必须 container stop + worktree remove（清理）
8. 最后才输出总结反馈

## 执行风格
- 第一个动作就是 bash 开 worktree（不要先分析）
- 改完一个文件就 container exec 验证一次（增量验证）
- 不停下来问"是否继续"
- bash 输出只看 | tail -10
```

### 2. 改 `src/agent_config.rs`（注册 improver 为 builtin）

在 `builtin_agents()` 函数（line 76）加一项，参照 plan agent 写法：

```rust
fn builtin_agents() -> Vec<AgentConfig> {
    let improver_md = include_str!("../examples/agents/improver.md");
    let improver = parse_agent_md("improver", improver_md)
        .unwrap_or_else(|| AgentConfig::simple("improver"));
    vec![
        build_builtin(), explore_builtin(), plan_builtin(),
        improver,  // ← 新增
    ]
}
```

改动 ~10 行。让 `ion --agent improver` 开箱即用，无需用户 cp 到 ~/.ion/agent/agents/。

### 3. 改 `src/agent/agent_loop.rs`（bash timeout 读环境变量）

line 989-993 改成读 `ION_TOOL_TIMEOUT`（默认 600）：

```rust
let long_timeout = std::env::var("ION_TOOL_TIMEOUT")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(600);
let timeout_duration = if tc_name == "skill" || tc_name == "bash" || tc_name == "bash_run" {
    std::time::Duration::from_secs(long_timeout)
} else {
    std::time::Duration::from_secs(120)
};
```

improver 跑时设 `ION_TOOL_TIMEOUT=1800`（30 分钟），container exec cargo build 不会超时。

### 4. 改 `src/agent/tool.rs`（BashTool timeout 也读环境变量）

line 561 + 578 的硬编码 `180` 改成读 `ION_BASH_TIMEOUT`（默认 180）：

```rust
let bash_to = std::env::var("ION_BASH_TIMEOUT")
    .ok().and_then(|s| s.parse().ok()).unwrap_or(180);
rt.execute_command(cmd, bash_to)
```

### 5. 新建 `docs/design/IMPROVER_AGENT.md`（设计文档）

用 DESIGN_TEMPLATE，记录：
- 架构图（host 改 + container 跑）
- 4 类话题分流
- 5 步核心流程（开 worktree → 改 → container 跑 → commit → 报告）
- container 不装 ion、worktree 挂载、不降级
- timeout 配置（ION_TOOL_TIMEOUT / ION_BASH_TIMEOUT）
- 失败处理（container 不可用就退出）

## 不改的东西

- **不改** `find_agent` 搜索路径（用 builtin 注册更干净）
- **不改** session_jsonl 路径逻辑（improver 在 host 跑，session 正常写到 ~/.ion/agent/sessions/）
- **不改** export.rs（resolve_session_file 已够用）
- **不改** Dockerfile.evolve（已有 Rust 环境，container 不需要 ion）
- **不改** init-evolve-container.sh（已支持 worktree 挂载）
- **保留** evolver.md + SELF_EVOLUTION.md（evolver 作为"自我进化专用版"继续存在，improver 是通用版）

## 验证方式

```bash
# 1. 确认 improver 注册成 builtin
ion --list-agents | grep improver

# 2. 调研类（不开 container，快）
ion --agent improver "分析 src/global_memory.rs 的搜索逻辑，找出潜在问题"

# 3. 改代码类（开 container，完整流程）
ION_TOOL_TIMEOUT=1800 ion --agent improver "给 global_memory 加 fn clear_all() 方法"

# 预期流程：
# bash: git worktree add → worktree 隔离
# read/edit: 在 worktree 改 global_memory.rs
# bash: init-evolve-container.sh → container 就绪
# bash: container exec cargo build → 编译验证
# bash: container exec cargo test → 测试验证
# bash: git commit
# bash: ion --export → HTML
# bash: container stop + worktree remove
# 输出: 报告路径 + 改动摘要

# 4. container 不可用
PATH=/usr/bin:/bin ion --agent improver "修个 bug"
# 预期：报错退出（找不到 container 命令）
```

## 风险评估

| 风险 | 等级 | 缓解 |
|------|------|------|
| builtin 注册影响现有 agent | 低 | builtin_agents() 独立 vec，不影响 find_agent |
| timeout 改动影响其他 agent | 低 | env var + 默认值，不设就跟原来一样 |
| container 不可用 | 中 | 按要求报错退出，文档说明前置条件 |
| improver 的 LLM 决策跑偏 | 中 | prompt 写"铁律" + bash 命令模板 |
| container 编译 OOM | 低 | init-evolve-container.sh 已限 --memory 4G，cargo build --bin ion 单 binary 不 OOM |