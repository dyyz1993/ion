# Self-Healing Pipeline — Monitor 驱动的自主修复闭环

> **状态：🔧 设计稿** — 设计完成，待 mock 验证 + 真实业务跑通。
> 基于 Monitor v2 + coordinator agent + 4 个 worker (developer/reviewer/merger/publisher)。

---

## 何时使用这个文档

- 把 monitor 事件 + coordinator + worker pipeline 串成端到端闭环时
- 设计"系统自己发现问题 → 自己修复 → 自己发布"的 autonomous workflow 时
- 对齐 pi 的 self-healing 概念（ION 原创实现，pi 没有）

**前置阅读**：
- [MONITOR_EXTENSION.md](./MONITOR_EXTENSION.md) — Monitor v2 系统
- [TEAM_ORCHESTRATION.md](./TEAM_ORCHESTRATION.md) — agent.md 驱动的多智能体
- examples/agents/coordinator.md — coordinator 已有的 monitor 事件响应章节

---

## 概览

把已有的零件串成一条**自主修复流水线**：

```
监控源（外部世界）
    │
    │ 每 N 秒拉取一次
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Monitor Extension (singleton)                                │
│  └─ 检测到状态变化 → emit monitor_triggered                  │
└─────────────────────────────────────────────────────────────┘
    │
    │ EventBus broadcast (visibility=LlmAndUi)
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Coordinator Agent                                            │
│  ├─ 接收事件                                                  │
│  ├─ 分析 data.output（issue / log / process state）         │
│  ├─ 决策：要不要 spawn worker？spawn 哪个？                  │
│  └─ 触发 pipeline                                            │
└─────────────────────────────────────────────────────────────┘
    │
    │ spawn_worker(developer, worktree=true, wait=true)
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Developer (worktree 隔离)                                    │
│  ├─ 分析问题（read source）                                  │
│  ├─ 修复代码（edit/write）                                   │
│  ├─ 跑测试（bash cargo test）                                │
│  └─ commit 到独立分支                                        │
└─────────────────────────────────────────────────────────────┘
    │
    │ spawn_worker(reviewer, wait=true)
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Reviewer (read-only)                                         │
│  ├─ git diff master...worktree-branch                        │
│  ├─ 检查清单（SQL / 错误处理 / 边缘 case / 测试 / UTF-8）   │
│  └─ 返回 APPROVE 或 REQUEST_CHANGES                          │
└─────────────────────────────────────────────────────────────┘
    │
    │ APPROVE  → 进入 merge
    │ REQUEST  → resume_worker(developer, fix issues) → 回到 Developer
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Merger                                                       │
│  ├─ git merge worktree-branch --no-edit                      │
│  ├─ git worktree remove                                      │
│  ├─ git branch -d                                            │
│  └─ 输出 merge commit                                        │
└─────────────────────────────────────────────────────────────┘
    │
    │ spawn_worker(publisher, wait=true)
    ▼
┌─────────────────────────────────────────────────────────────┐
│ Publisher                                                    │
│  ├─ git push origin master                                   │
│  ├─ gh issue close <number>  （如果是 issue 触发）           │
│  └─ 可选：gh pr create / release create                      │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
完成。Coordinator 记录结果 + 进 standby 等下一个事件。
```

### 能力清单

| 能力 | 入口 | 状态 |
|------|------|------|
| Monitor 检测 + emit 事件 | `monitor.json` + interval loop | ✅ v2 已实现 |
| Coordinator 接收事件 + 决策 | coordinator.md §monitor 事件处理 | ✅ 已实现 |
| Coordinator spawn developer | `spawn_worker(developer, worktree=true, wait=true)` | ✅ 工具就绪 |
| Developer 修复 + commit | examples/agents/developer.md | ✅ 已实现 |
| Reviewer 审查 + APPROVE/REQUEST | examples/agents/reviewer.md | ✅ 已实现 |
| Merger 合并 + cleanup worktree | examples/agents/merger.md | ✅ 已实现 |
| Publisher push + close issue | examples/agents/publisher.md | ✅ 已实现 |
| **端到端闭环**（本设计） | coordinator.md §self-healing pipeline 章节 ✅ 已补 | ✅ 已实现 |

---

## 1. 配置

### 1.1 触发配置：让 monitor 推 message 给 coordinator

监控触发后**必须让 coordinator 知道**。两种方式：

**方式 A：trigger_mode = channel_notify**（推荐）

```json
{
  "name": "github-issues",
  "interval_secs": 300,
  "script": "gh issue list --repo X --label bug --json number,title 2>/dev/null",
  "agent": "coordinator",
  "prompt_template": "New bug issues:\n{output}\n\nProcess each via self-healing pipeline.",
  "mode": "serial_skip",
  "trigger_mode": "channel_notify"
}
```

Monitor 把消息推到 `main` channel，订阅了 main 的 coordinator 收到。

**方式 B：trigger_mode = event_only + coordinator 监听**

```json
{"trigger_mode": "event_only", ...}
```

Coordinator 作为 subscriber，通过 EventBus 接收 `monitor_triggered` 事件。当前实现下需要 coordinator 主动 `subscribe` 才能 async 收事件，对 LLM 来说复杂。**先用方式 A**。

### 1.2 Coordinator 启动配置

```bash
ion serve &
# Or scene 2:
ion --host --agent coordinator "Enter standby mode. Subscribe to main channel. Wait for monitor events."
```

coordinator 启动后：
- 订阅 `main` channel（agent.md 已配置）
- 不做任何主动工作（避免污染）
- 等 monitor 推消息

---

## 2. 主流程（self-healing pipeline）

### 2.1 Coordinator 收到 monitor_triggered 后的伪代码

```python
# 在 coordinator.md 的 §monitor 事件处理 章节基础上扩展
async def handle_monitor_triggered(event):
    data = event.data
    monitor_name = data.name
    output = data.output
    trigger_mode = data.trigger_mode

    # Step 1: dedup 检查（避免重复 spawn）
    if active_pipeline_exists(monitor_name, output):
        log("skip: pipeline already running for this issue")
        return

    # Step 2: 分析 output，决定要不要触发 pipeline
    issues = parse_issues(output)  # 假设是 GitHub issue 列表
    if not issues:
        log("no actionable issues")
        return

    # Step 3: 对每个 issue 启动一个 pipeline
    for issue in issues:
        # 记录 active pipeline（用于 dedup）
        record_active(monitor_name, issue.number)

        # ── pipeline 开始 ──
        # 3a: developer 修复
        dev = spawn_worker(
            relation="child",
            agent="developer",
            task=f"Fix issue #{issue.number}: {issue.title}\n"
                 f"Context: {issue.body}\n"
                 f"Work in worktree. Commit when done.",
            worktree=True,
            wait=True   # 同步等 developer 完成
        )

        if dev.failed:
            log(f"developer failed: {dev.error}")
            release_active(monitor_name, issue.number)
            continue

        # 3b: reviewer 审查（最多 3 轮）
        for round in 1..=3:
            review = spawn_worker(
                relation="child",
                agent="reviewer",
                task=f"Review changes in worktree branch from issue #{issue.number}.\n"
                     f"git diff master...HEAD\n"
                     f"Report APPROVE or REQUEST_CHANGES with specific issues.",
                wait=True
            )

            if review.verdict == "APPROVE":
                break
            elif review.verdict == "REQUEST_CHANGES":
                # 让 developer 修
                resume_worker(dev.worker_id, f"Reviewer feedback:\n{review.issues}\nPlease fix.")
                # 等 developer 完成
                await_worker(dev.worker_id)
            else:
                log(f"reviewer unclear: {review}")
                break

        if review.verdict != "APPROVE":
            log(f"could not converge after 3 rounds, skip merge")
            kill_worker(dev.worker_id)
            release_active(monitor_name, issue.number)
            continue

        # 3c: merger 合并
        merge = spawn_worker(
            relation="child",
            agent="merger",
            task=f"Merge worktree branch for issue #{issue.number} into master.\n"
                 f"Cleanup worktree after merge.",
            wait=True
        )

        if merge.failed:
            log(f"merger failed: {merge.error}")
            continue

        # 3d: publisher 发布
        publish = spawn_worker(
            relation="child",
            agent="publisher",
            task=f"Push master to origin. Close issue #{issue.number} with comment 'Fixed in {merge.commit}'.",
            wait=True
        )

        # 3e: 清理 dedup 记录
        release_active(monitor_name, issue.number)
        log(f"✅ issue #{issue.number} resolved")
```

### 2.2 关键决策点

| 场景 | 处理 |
|------|------|
| 同一 monitor 在 pipeline 跑期间再次触发 | coordinator 检查 `active_pipeline`，skip |
| Developer 修复失败（compile error / 测试不过） | 不进 review，直接 log + release_active |
| Reviewer 3 轮都 REQUEST_CHANGES | kill developer，放弃该 issue（避免死循环） |
| Merger 失败（冲突） | 不进 publisher，log + manual intervention |
| Publisher 失败（网络） | log，但 merge 已经在本地完成，下次重试 push |
| 同一时间多个 issue | **不并行**（serial_skip 模式），逐个处理 |

### 2.3 防 dedup 机制

```rust
// 用 monitor name + issue number 作 key
fn active_pipeline_exists(monitor: &str, output: &str) -> bool {
    let issues = parse_issues(output);
    let lockfile = format!("/tmp/heal-active-{monitor}.txt");
    let active: HashSet<String> = read_lockfile(&lockfile);
    issues.iter().any(|i| active.contains(&format!("issue-{}", i.number)))
}

fn record_active(monitor: &str, issue_num: u64) {
    append_to_lockfile(
        &format!("/tmp/heal-active-{monitor}.txt"),
        &format!("issue-{issue_num}\n")
    );
}

fn release_active(monitor: &str, issue_num: u64) {
    remove_from_lockfile(
        &format!("/tmp/heal-active-{monitor}.txt"),
        &format!("issue-{issue_num}")
    );
}
```

简化版（coordinator 不直接读写 lockfile，而是用 send_to_worker 通知自己的 peer worker 维护状态）：

```python
# coordinator 把 active state 存在自己的对话里
self.messages.append({"role": "system", "content": f"ACTIVE: {monitor}/issue-{num}"})
```

---

## 3. Mock 验证（不靠 LLM）

为了确定性验证 pipeline 串得对，用 mock 替代真实 LLM：

### 3.1 Mock setup

```bash
# 1. 启 serve
ion serve &

# 2. Mock gh（推一个 fake issue）
mkdir -p /tmp/mock-gh-dir
cat > /tmp/mock-gh-dir/gh <<'EOF'
#!/bin/sh
case "$1 $2" in
  "issue list") echo '[{"number":42,"title":"test: pipeline trigger"}]';;
  "issue close") echo "Closed";;
  *) echo "";;
esac
EOF
chmod +x /tmp/mock-gh-dir/gh

# 3. 配置 monitor（用 channel_notify 模式）
cat > .ion/monitors/test.json <<'EOF'
{
  "name": "test-pipeline",
  "interval_secs": 30,
  "script": "PATH=/tmp/mock-gh-dir:$PATH gh issue list --repo test/test --json number,title 2>/dev/null",
  "agent": "coordinator",
  "prompt_template": "Issues to process via self-healing pipeline:\n{output}",
  "mode": "serial_skip",
  "trigger_mode": "channel_notify"
}
EOF

# 4. spawn coordinator（订阅 main channel）
ion rpc --method create_session --params '{"agent":"coordinator","channels":["main"]}'
```

### 3.2 验证步骤

```bash
# Subscribe 看 pipeline 进度
ion subscribe > /tmp/heal-pipeline.log &

# 等 monitor 触发（30s）
sleep 35

# 检查事件流
grep -E "monitor_triggered|monitor_channel_notify" /tmp/heal-pipeline.log
grep -E "agent_start|text_delta" /tmp/heal-pipeline.log | head -20

# 检查 coordinator 是否真的 spawn 了 developer
ion rpc --method list_workers --params '{}'
# 预期：除了 coordinator，还有 developer (worktree 分支)
```

---

## 4. 真实业务 case

### Case 1: GitHub issue 自动修复

```bash
# 1. scheduler 生成 monitor（用 skill）
ion --agent build --skill scheduler \
    "监控 dyyz1993/ion 仓库的 bug issue，每 10 分钟一次。trigger_mode=channel_notify，让 coordinator 处理。"

# 2. 启动 serve + coordinator
ion serve &
ion rpc --method create_session --params '{"agent":"coordinator","channels":["main"]}'

# 3. 在 GitHub 上开一个新 bug issue
gh issue create --repo dyyz1993/ion --title "bug: ..." --body "..."

# 4. 等 10 分钟，看 pipeline 自动跑
ion subscribe  # 实时看 monitor_triggered → developer spawn → reviewer → merger → publisher
```

### Case 2: 日志错误自动修复

```json
{
  "name": "log-errors",
  "interval_secs": 60,
  "script": "grep -E 'panic|FATAL' /var/log/ion.log 2>/dev/null | tail -5",
  "agent": "coordinator",
  "prompt_template": "Log errors detected:\n{output}\nTrace source and fix via pipeline.",
  "trigger_mode": "channel_notify"
}
```

### Case 3: 测试失败自动修复

```json
{
  "name": "test-failures",
  "interval_secs": 300,
  "script": "cd /path/to/project && cargo test 2>&1 | grep -E 'FAILED|test result.*failed'",
  "agent": "coordinator",
  "prompt_template": "Test failures:\n{output}\nFix failing tests via pipeline.",
  "trigger_mode": "channel_notify"
}
```

---

## 5. Coordinator prompt 改造

需要在 `examples/agents/coordinator.md` 新增 **§self-healing pipeline** 章节，让 LLM 真的按这个流程走。

### 新增内容（追加到现有 monitor 事件处理章节之后）

```markdown
## Self-healing pipeline（monitor → developer → reviewer → merger → publisher）

当 monitor_triggered 事件包含可处理的数据（issue / error / test failure）时，
启动完整 pipeline：

### Pipeline 阶段

1. **分析**：解析 data.output，提取需要处理的项目（issue number / error line / test name）
2. **Dedup**：检查 `self.messages` 里是否有相同 issue number 的 ACTIVE 标记
3. **Developer**：spawn_worker(child, developer, worktree=true, wait=true,
   task="Fix issue #N: <title>. <body>"),
   等返回（dev commit 在 worktree 分支）
4. **Reviewer**（最多 3 轮）：spawn_worker(child, reviewer, wait=true,
   task="Review git diff master...HEAD. APPROVE or REQUEST_CHANGES with issues."),
   - APPROVE → 进 merger
   - REQUEST_CHANGES → resume_worker(developer, "fix: <issues>") → await → 回 reviewer
5. **Merger**：spawn_worker(child, merger, wait=true,
   task="Merge worktree branch + cleanup")
6. **Publisher**：spawn_worker(child, publisher, wait=true,
   task="git push origin master + gh issue close #N with merge commit")
7. **Record**：在 self.messages 添加 "RESOLVED: monitor-name/issue-N"

### Anti-patterns（避免）

- ❌ 同一 issue 被 monitor 多次触发时重复 spawn developer
- ❌ Reviewer 反复 REQUEST_CHANGES 不收敛（最多 3 轮，超过放弃）
- ❌ 同一时间处理多个 issue（serial_skip 模式保证一次一个）
- ❌ publisher 失败时回滚 merger（merge 已在本地，下次重试 push 即可）

### Convergence 规则

- 3 轮 review 失败 = 放弃该 issue，log + 移交人工
- Developer compile 失败 = 不进 review，直接放弃
- Merger 冲突 = 不进 publisher，log + 移交人工
```

---

## 6. 已知缺口（后续补）

| # | 缺口 | 影响 | 优先级 |
|---|------|------|--------|
| 1 | Coordinator 持久化 `self.messages` 在重启后丢失（active pipeline 状态丢） | 重启后可能重复 spawn | P1 |
| 2 | Reviewer 反馈格式没标准化（APPROVE/REQUEST_CHANGES 检测靠 LLM 解析） | 偶尔 LLM 输出怪格式 | P2 |
| 3 | 没有上限控制（一个 issue 跑半小时，monitor 30s 一次，dedup 文件膨胀） | 文件越来越大 | P2 |
| 4 | Pipeline 中途 serve 崩溃 = 全部丢（无 checkpoint） | 长任务可能白跑 | P3 |
| 5 | Publisher push 失败时 merger 已 commit，没自动重试机制 | push 偶尔失败需要人工 | P2 |

---

## 7. 后续工作

| # | 待办 | 优先级 | 依赖 |
|---|------|--------|------|
| 1 | 在 coordinator.md 加 §self-healing pipeline 章节 | P0 | — |
| 2 | mock 验证（3.2 节） | P0 | 1 |
| 3 | 真实 GitHub issue 端到端跑通 | P1 | 1, 2 |
| 4 | 解决缺口 1（持久化 active state） | P1 | 3 |
| 5 | 解决缺口 2（reviewer 格式标准化） | P2 | 3 |
| 6 | 加 workflow.yaml 版本（结构化 pipeline） | P3 | 3 |
