# ION 自我进化闭环 — 设计文档

> **状态：开发中** — evolver agent + init-evolve-container.sh 已创建，待端到端验证。

---

## 1. 核心理念

ION 能**自主改进自己的代码**——不是靠人改，而是 ION 自己在隔离空间里改代码、测试、开 PR，用户最终决定是否合并。

**不是每次进化都成功**——失败的 worktree 直接丢弃，主分支不受影响。

---

## 2. 四层身份

| 层 | 身份 | 角色 | 隔离 |
|----|------|------|------|
| ① | 用户 | 给任务 + 最终决定是否合并 | — |
| ② | ZCode（LLM 编程工具） | 调用 ION CLI + 给用户汇报 | — |
| ③ | ION 主体 | evolver agent，用 bash 编排全流程 | 主分支 |
| ④ | ION 衍生的独立空间 | worktree + Apple Container，跑 developer/reviewer | 双重隔离 |

```
① 用户："修复 uuid 碰撞 bug"
  ↓
② ZCode: ion --host --agent evolver "修复 uuid 碰撞 bug"
  ↓
③ ION 主体（evolver agent）:
  ↓ bash: git worktree add → 代码隔离
  ↓ bash: scripts/init-evolve-container.sh → container 隔离
  ↓ bash: container exec ... ion --host --agent developer → 改代码
  ↓ bash: container exec ... cargo test → 测试
  ↓ bash: container exec ... ion --host --agent reviewer → 审查
  ↓ bash: ion --export → HTML 报告
  ↓ bash: gh pr create → 开 PR
  ↓ bash: container stop + git worktree remove → 清理
  ↓ 返回: PR URL + 报告路径
  ↓
② ZCode: 打开 HTML 报告给用户
  ↓
① 用户: 看报告 → 在 GitHub 上 merge PR（或不 merge）
```

---

## 3. 完整 11 步流程

| 步骤 | 动作 | 执行者 | 隔离 |
|------|------|--------|------|
| ① | 用户给出进化任务 | 用户 | — |
| ② | ZCode 调用 ION | ZCode | — |
| ③ | ION 接收任务（evolver agent 启动） | ION 主体 | — |
| ④ | 开 worktree + container 隔离空间 | evolver（bash） | ④层 |
| ⑤ | developer agent 改代码 | ION 子实例 | ④层 |
| ⑥ | cargo test 验证 | evolver（bash） | ④层 |
| ⑦ | reviewer agent 深度审查 | ION 子实例 | ④层 |
| ⑧ | 深度测试循环（CI 脚本） | reviewer | ④层 |
| ⑨ | 生成 HTML 报告 + 开 PR | evolver（bash） | ③→② |
| ⑩ | ZCode 收到结果，打开报告给用户 | ZCode | — |
| ⑪ | 用户看报告，在 GitHub 决定是否 merge | 用户 | — |

---

## 4. 关键设计点

### 4.1 双重隔离

- **worktree**：`git worktree add` 创建独立分支 + 目录。改坏了直接 `git worktree remove --force`，主分支不受影响。
- **Apple Container**：`container run` 创建 Linux VM。container 里的 cargo test / ion 进程完全隔离，不影响 host 系统。container `--rm` 退出即删。

### 4.2 bash 串行执行（不用 spawn_worker）

evolver 不用 `spawn_worker`——所有操作通过 bash 串行执行：

```bash
# bash 1: 开 worktree
bash: git worktree add "$WT_DIR" -b evolve/xxx

# bash 2: 开 container
bash: scripts/init-evolve-container.sh "$WT_DIR"

# bash 3: container 里编译（阻塞 10-20 分钟）
bash: container exec $NAME sh -c 'cd /workspace && cargo build'

# bash 4: container 里改代码（阻塞 10-20 分钟）
bash: container exec $NAME sh -c 'cd /workspace && ion --host --agent developer "..."'

# bash 5: container 里测试（阻塞 5-10 分钟）
bash: container exec $NAME sh -c 'cd /workspace && cargo test --lib'
```

每一步等 bash 返回后再做下一步。bash 超时设 600s（10 分钟）。

### 4.3 Container 里跑 ION

第⑤步不是 evolver 自己改代码——是在 container 里启动一个**全新的 ION 实例**（场景 2 `ion --host`），让 developer agent 干活：

```bash
container exec $NAME sh -c '
  cd /workspace &&
  ./target/release/ion --host --agent developer "修复 uuid 碰撞"
'
```

这启动了一个完整的 ION host + Worker，developer agent 在隔离的 container 里自由读/写/编译。

### 4.4 用户最终决定

ION 不自动合并。只开 PR（`gh pr create`），用户在 GitHub 上 review + merge。失败的进化直接丢弃 worktree。

### 4.5 HTML 报告

进化完成后导出 HTML（`ion --export`），包含完整流程：
- developer 改了什么（read/edit 工具调用）
- cargo test 结果（bash 输出）
- reviewer 评价（APPROVE / REQUEST_CHANGES）
- PR 链接

---

## 5. Container 环境要求

### 5.1 Docker 镜像

```dockerfile
# rust:latest 包含：
# - rustc + cargo（Rust 编译器）
# - git（代码版本控制）
# - curl + apt（安装额外工具）
# 不包含：
# - gh CLI（需额外安装）
# - ion binary（需从 host 复制或在 container 内编译）
```

### 5.2 环境初始化

使用 `scripts/init-evolve-container.sh` 一键初始化：

```bash
bash scripts/init-evolve-container.sh /path/to/worktree
```

脚本自动：
1. 检查 Apple Container 系统
2. 创建 container（rust:latest）
3. 挂载 worktree 到 /workspace
4. 验证 rustc/cargo/git 可用
5. 复制 ion binary（如果有）
6. 安装 gh CLI
7. 输出 CONTAINER_NAME

### 5.3 资源限制

```bash
--memory 4G    # Rust 编译需要足够内存
--cpus 4       # 并行编译加速
```

---

## 6. 文件清单

| 文件 | 说明 |
|------|------|
| `scripts/init-evolve-container.sh` | Container 环境初始化脚本 |
| `examples/agents/evolver.md` | 自我进化编排 agent（prompt 定义） |
| `docs/design/SELF_EVOLUTION.md` | 本设计文档 |
| `AGENTS.md` | 链接到本文档 + container 环境说明 |

---

## 7. 失败处理

| 场景 | 处理 |
|------|------|
| cargo build 失败 | developer 修复 → 重试（最多 3 次） |
| cargo test 失败 | developer 修复 → 重试（最多 3 次） |
| reviewer REQUEST_CHANGES | developer 按建议修复 → 重试（最多 3 次） |
| 超过重试限制 | 报告失败 + 清理 worktree + container |
| container 启动失败 | 报告错误，不继续 |
| git push 失败 | 报告错误，worktree 保留供用户手动检查 |

所有失败都会清理 container（`container stop`），失败的 worktree 可选保留（供用户排查）或删除。

---

## 8. 未来扩展

- **自动触发**：Hooks 系统 Stop 事件 → 自动检测可改进的代码 → 触发进化
- **学习记忆**：失败的进化记录到 global_memory → 下次避免同样错误
- **并行进化**：多个 worktree + container 同时跑不同改进
- **回归保护**：每次进化跑全套 CI（所有 `*_ci.sh`），确保不破坏
