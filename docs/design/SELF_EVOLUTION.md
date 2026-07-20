# ION 自我进化 — A 驱动 B 架构

> **状态：开发中** — init-evolve-container.sh + evolver.md 已实现，待端到端验证。

## 一句话

A（host 上的 ION）驱动 B（container 里的 ION）改代码 + 跑 CI，CI 通过后 A 从 B 拉代码回来。A 自己绝不碰自己的代码。

## 核心架构

```
ZCode（用户）
  │
  │  ion --host --agent evolver "给 xxx 加方法"
  │  （ZCode 给 A 任务）
  │
  ▼
A = ION 主体（host 上，agent 角色 = evolver）
  │
  │  A 对 B 做的事，跟 ZCode 对 A 做的事一模一样：
  │
  │  1. git worktree add（开隔离空间）
  │  2. init-evolve-container.sh（启动 B + 编译 ion）
  │  3. container exec B ion --agent developer "任务"（调用 B 改代码）
  │  4. container exec B cargo test（B 自己跑 CI）
  │  5. 看 CI 结果，失败重试（回到 3），通过进 6
  │  6. git merge worktree 分支（A 从 B 拉代码回来）
  │  7. container stop + worktree remove（清理）
  │
  ▼
B = ION 衍生体（container 里，完整 ION 实例）
  │
  │  B 有自己的 ion + LLM + 工具 + CI 脚本
  │  B 自己改代码、自己 commit、自己跑测试
  │  B 的改动通过 bind-mount 自动同步到 host worktree
  │
  ▼
A 的代码进化了（B 里验证过的才合并回来，A 自己从没改过代码）
```

## 为什么这是正确的架构

| 原则 | 说明 |
|------|------|
| **A 绝不碰自己代码** | 就像 ZCode 不碰 ION 源码一样——A 只调度，不执行 |
| **B 是完整 ION 实例** | 有 ion binary + LLM + 工具 + CI，跟 A 一样的能力 |
| **A 通过"调用"驱动 B** | `container exec B ion --agent developer "..."`，跟 ZCode 调用 A 完全一样 |
| **B 自己跑 CI** | B 里的 ion 自己跑 cargo test，不是 A 替 B 跑 |
| **CI 通过后 A 拉代码** | bind-mount 让 B 的 commit 自动同步到 host worktree，A git merge 即可 |

## 关键技术实现

### container 里的 ion binary

host 是 macOS arm64，container 是 Linux VM——跨架构 binary 跑不了。所以 **ion 在 container 内编译**：

```bash
container exec $NAME sh -c 'cd /workspace && cargo build --release --bin ion'
```

首次编译 10-20 分钟，后续 incremental 快。

### API key + 配置传递

挂载 `~/.ion` 到 container（只读）：
```bash
container run -v ~/.ion:/root/.ion:ro ...
```

B 能读 host 的 config.json / auth.json / models.json / skills/——零配置。

### 代码同步（bind-mount）

worktree 挂载到 container 的 /workspace：
```bash
container run -v $WT_DIR:/workspace ...
```

B 在 container 里 `git commit`，改动直接落到 host 的 worktree 目录。A 不需要 git pull——`cd $WT_DIR && git log` 立刻可见。

## A 的 agent（evolver.md）

```yaml
tools:
  - read
  - ls
  - grep
  - find
  - bash
disallowed_tools:
  - edit    # A 不能改代码
  - write   # A 不能写代码
```

A 的 prompt 铁律：
1. A 绝不 edit/write
2. A 绝不在 host 上 cargo build/test
3. 改代码必须 `container exec B ion --agent developer "..."`
4. CI 必须在 B 里跑

## B 的 agent（container 里的 ion）

B 用 `--agent developer` 启动，developer.md 定义了 B 的行为：
- read 源文件
- edit 改代码
- bash git add + commit
- bash cargo build/test

B 不知道自己在 container 里——对 B 来说，它就是一个普通的 ION agent 在改代码。

## 废弃的旧方案

以下方案是"A 自己编排自己改自己"的错误方向，已废弃：

- `scripts/improver.sh` — 外部脚本编排 A（错误：A 应该自己驱动 B，不是被外部脚本编排）
- `.ion/workflows/improver.wf.yaml` — workflow 外挂（错误：workflow 应该在 agent 内部）
- `examples/agents/improver.md` — 改成 workflow 引擎（错误：A 不应该自己做 workflow）

正确方案是 `examples/agents/evolver.md`——A 的 agent 直接具备"驱动 B"的能力。

## 验证方式

```bash
# 端到端验证
ion --host --agent evolver "给 global_memory 加 fn last_count() 方法"
# 预期：
# A 开 worktree → 启 container → 编译 ion → 调 B 改代码 → B 自己 commit
# → B 自己跑 cargo test → A 看结果 → 合并到 master → 清理
```

## 相关文件

| 文件 | 作用 |
|------|------|
| `examples/agents/evolver.md` | A 的 agent 定义 |
| `scripts/init-evolve-container.sh` | 启动 B（container + 编译 ion） |
| `scripts/Dockerfile.evolve` | container 镜像（Rust 工具链） |
| `examples/agents/developer.md` | B 的 agent 定义（container 里用） |
