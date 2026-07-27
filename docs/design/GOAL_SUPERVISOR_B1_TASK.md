# Goal Supervisor B1 — A→B 任务规格包

> **用途**：用 A→B 自进化架构实现 Goal Supervisor Phase 1（核心闭环）。
> ZCode 用后台 Bash 启动 `ion --host --agent coordinator`，传入本规格。

---

## 0. ZCode 启动方式（铁律：必须后台）

```bash
# 1. 先启 container + worktree（首次约 3 分钟）
ION_TOOL_TIMEOUT=1800 bash scripts/evolve.sh

# 2. 加载状态
source /tmp/.evolver-state

# 3. 后台启动 A→B（ZCode 必须用 run_in_background: true）
cat <<'PROMPT' | container exec -i "$CONTAINER_NAME" sh -c \
  "cd /workspace && ./target/release/ion --host --agent coordinator \
   --provider zai --model glm-5.2" &
PROMPT
实现 Goal Supervisor Phase 1（核心闭环）。

完整规格在 docs/design/GOAL_SUPERVISOR.md 和 GOAL_SUPERVISOR_B1_TASK.md。
请先 read 这两个文件，然后按 B1 范围拆 task 派给 developer。

验收标准：
1. cargo build 通过
2. cargo test --lib goal_supervisor 全过
3. cargo clippy 无新增 warning
4. grep -c U+FFFD src/ 返回 0
5. tests/goal_supervisor_ci.sh 跑通基础场景（A1+A2）

完成后在主仓库 commit：feat(goal): Goal Supervisor B1 — core loop + checks + guards
PROMPT
```

---

## 1. B1 范围（必须做 + 不做）

### ✅ 必须做

| 项 | 文件 | 说明 |
|----|------|------|
| **GoalSupervisorExtension** | `src/goal_supervisor_extension.rs`（新增） | 主结构 + 状态机 + 6 道防线 |
| **goal_set tool** | 同上 | 暴露给 LLM 的唯一 tool |
| **Check / CheckResult / Evidence** | 同上 | 数据结构 |
| **run_all_checks** | 同上 | 确定性执行检测项 + 收证据 |
| **日志** | 同上 | iterations.jsonl + goal.json |
| **注册** | `src/agent/extension.rs` | 加入 ExtensionRegistry |
| **config** | `src/config.rs`（或现有 config 结构） | `goal_supervisor.enabled` 开关 |
| **单元测试** | 同 `src/goal_supervisor_extension.rs` 底部 | 状态机/防线/日志 schema |
| **harness 脚本** | `tests/goal_supervisor_ci.sh` | 场景 A1/A2/B1（FauxProvider） |

### ❌ B1 不做（留给 B2/B3）

- ❌ GOAL skill + rust-ci skill（B2）
- ❌ outcome 回填（B2）
- ❌ goal-evolver agent（B3）
- ❌ `goal_evolver_run_once` RPC（B3）
- ❌ 检测项自动生成（B1 先支持用户/测试**手动传 checks**，自动生成留 B2）

### B1 的闭环简化版

B1 不依赖 skill 生成检测项，**`goal_set` 必须带 checks**（测试时手动构造）。闭环是：

```
goal_set(objective, checks=[...])
  → on_agent_end → run_all_checks（确定性执行）
    → 全 PASS → complete
    → 有 FAIL → 注入 continue message（带失败证据）→ 下一轮
  → 6 道防线兜底
```

---

## 2. 给 developer 的实现 spec

### 2.1 数据结构（`src/goal_supervisor_extension.rs`）

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::Mutex;
use std::sync::Arc;

// ── 检测项 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckType { Ci, Contingency }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PassCriteria {
    /// exit_code == expected
    ExitCode { expected: i32 },
    /// grep pattern 在文件/输出中匹配数 == 0（用于 U+FFFD 检测）
    GrepEmpty { pattern: String },
    /// 文件存在
    FileExists { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub check_type: CheckType,
    pub rationale: String,
    pub command: String,
    pub pass_criteria: PassCriteria,
    pub must_pass: bool,
}

// ── 检测结果 + 证据 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckStatus { Pass, Fail, Error, Skipped }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub exit_code: Option<i32>,
    pub stdout_excerpt: Option<String>,  // 截断到前 2000 字符
    pub artifact_path: Option<String>,   // 完整日志路径
    pub matches: Option<Vec<String>>,    // grep 匹配行
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub evidence: Option<Evidence>,  // None = 无证据，视为 Fail
    pub duration_ms: u64,
    pub reason: Option<String>,
}

// ── Goal 状态机 ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GoalStatus {
    Running,
    Checking,
    Complete,
    Exhausted,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub goal_id: String,
    pub objective: String,
    pub checks: Vec<Check>,
    pub status: GoalStatus,
    pub iteration_count: u32,
    pub started_at: u64,           // unix ms
    pub total_cost_usd: f64,
    pub last_action_plan: Option<String>,  // 用于重复检测
}

// ── 配置 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSupervisorConfig {
    pub enabled: bool,
    pub check_on_agent_end: bool,
    pub max_iterations: u32,
    pub max_total_duration_min: u32,
    pub max_total_cost_usd: f64,
    pub repetition_threshold: f64,
    pub delay_ms: u64,
}
impl Default for GoalSupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_on_agent_end: true,
            max_iterations: 20,
            max_total_duration_min: 30,
            max_total_cost_usd: 5.0,
            repetition_threshold: 0.8,
            delay_ms: 5000,
        }
    }
}
```

### 2.2 Extension 主体（骨架）

```rust
pub struct GoalSupervisorExtension {
    state: Arc<Mutex<Option<GoalState>>>,
    config: GoalSupervisorConfig,
    session_id: String,
}

#[async_trait::async_trait]
impl AgentExtension for GoalSupervisorExtension {
    fn name(&self) -> &str { "goal_supervisor" }

    async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
        if !self.config.enabled { return Ok(()); }
        let has_goal = self.state.lock().await.is_some();
        if !has_goal { return Ok(()); }

        // 1. 跑检测
        let results = self.run_all_checks(ctx).await?;

        // 2. 记日志
        self.log_iteration(&results).await?;

        // 3. 判定
        let all_pass = results.iter().all(|r| r.status == CheckStatus::Pass);
        if all_pass {
            self.set_status(GoalStatus::Complete).await;
            self.write_final_report("complete", "all_checks_passed").await?;
            return Ok(());
        }

        // 4. 防线检查
        if let Some(reason) = self.check_guards().await? {
            self.set_status(GoalStatus::Exhausted).await;
            self.write_final_report("exhausted", &reason).await?;
            return Ok(());
        }

        // 5. 注入 continue message（带失败证据）
        self.inject_continue(&results).await?;
        Ok(())
    }
}

impl GoalSupervisorExtension {
    /// 执行所有检测项，收证据
    async fn run_all_checks(&self, ctx: &AgentContext) -> AgentResult<Vec<CheckResult>> {
        let state = self.state.lock().await;
        let checks = state.as_ref().unwrap().checks.clone();
        drop(state);

        let mut results = Vec::new();
        for check in &checks {
            let result = self.run_single_check(check).await?;
            results.push(result);
        }
        Ok(results)
    }

    async fn run_single_check(&self, check: &Check) -> AgentResult<CheckResult> {
        // 用 tokio::process::Command 执行 check.command
        // 根据 pass_criteria 判定
        // 收集 evidence（exit_code + stdout 写到 artifact 文件）
        // 无 evidence → status=Fail
        todo!()
    }

    /// 6 道防线
    async fn check_guards(&self) -> AgentResult<Option<String>> {
        let state = self.state.lock().await;
        let s = state.as_ref().unwrap();
        // ① max_iterations
        if s.iteration_count >= self.config.max_iterations {
            return Ok(Some("max_iterations".into()));
        }
        // ⑤ 时长上限
        let elapsed_min = (now_ms() - s.started_at) / 60000;
        if elapsed_min >= self.config.max_total_duration_min as u64 {
            return Ok(Some("max_duration".into()));
        }
        // ⑥ 成本上限
        if s.total_cost_usd >= self.config.max_total_cost_usd {
            return Ok(Some("max_cost".into()));
        }
        // ③ 重复检测（text similarity）
        // ... calculateTextSimilarity 对比 last_action_plan
        Ok(None)
    }

    /// 注入 continue message
    async fn inject_continue(&self, results: &[CheckResult]) -> AgentResult<()> {
        let failed: Vec<_> = results.iter()
            .filter(|r| r.status != CheckStatus::Pass)
            .collect();
        let msg = format!(
            "目标未完成。以下检测项 FAIL：\n{}",
            failed.iter().map(|r| format!(
                "- {} (证据: {})", r.name,
                r.evidence.as_ref().map(|e| e.stdout_excerpt.as_deref().unwrap_or("N/A")).unwrap_or("无")
            )).collect::<Vec<_>>().join("\n")
        );
        // 通过 ctx 注入到 agent 的下一轮（参考 hooks 的 prompt handler 注入机制）
        todo!()
    }
}
```

### 2.3 goal_set tool

```rust
pub struct GoalSetTool { /* shared state with extension */ }

#[async_trait::async_trait]
impl Tool for GoalSetTool {
    fn name(&self) -> &str { "goal_set" }

    fn description(&self) -> &str {
        "设置或覆盖当前会话的目标。设置后，每次 agent 结束会自动跑检测项，没全 PASS 就继续执行，直到目标完成或触发防线。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "objective": { "type": "string" },
            "checks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "check_type": {"enum": ["ci", "contingency"]},
                        "command": {"type": "string"},
                        "pass_criteria": { /* tagged union */ }
                    }
                }
            }
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult, String> {
        let objective = params["objective"].as_str().ok_or("objective required")?;
        let checks: Vec<Check> = serde_json::from_value(params["checks"].clone())
            .map_err(|e| e.to_string())?;

        // 覆盖旧 goal（如果有，标记 cancelled）
        self.set_goal(objective, checks).await;
        Ok(ToolResult::text(format!("目标已设置：{}", objective)))
    }
}
```

### 2.4 日志 schema（严格按 GOAL_SUPERVISOR.md §7）

写到 `~/.ion/agent/goal-runs/<session_id>/iterations.jsonl`，每行一个 JSON。
字段必须完整（guards_hit / checks_run / all_passed / failed_checks）。

### 2.5 注册（`src/agent/extension.rs`）

- 在 ExtensionRegistry 初始化处，读 config.goal_supervisor.enabled
- enabled=true 时注册 GoalSupervisorExtension + GoalSetTool
- 复用 SharedPlanExtension 的共享模式（extension 和 tool 共享 state）

---

## 3. 守门清单（A 必查，B commit 前自查）

```
① grep -c $'\xef\xbf\xbd' src/goal_supervisor_extension.rs   # 必须返回 0（U+FFFD）
② cargo build                                                 # 必须通过
③ cargo test --lib goal_supervisor                            # 必须全过
④ cargo clippy -- -D warnings 2>&1 | grep goal_supervisor     # 无新增 warning
⑤ cargo fmt --check                                           # 格式正确
⑥ 不修改 Cargo.toml（不加新依赖，除非绝对必要且 A 确认）
⑦ comment 全英文（铁律，防 U+FFFD）
```

---

## 4. 测试要求

### 4.1 单元测试（`src/goal_supervisor_extension.rs` 底部，`#[cfg(test)]`）

至少覆盖：

| 测试 | 验证 |
|------|------|
| `test_check_exit_code_pass` | ExitCode(0) + command 成功 → Pass |
| `test_check_exit_code_fail` | ExitCode(0) + command 失败 → Fail |
| `test_check_grep_empty_pass` | GrepEmpty + 无匹配 → Pass |
| `test_check_grep_empty_fail` | GrepEmpty + 有匹配 → Fail（用于 U+FFFD） |
| `test_check_no_evidence_is_fail` | command 执行无产出 → status=Fail |
| `test_goal_set_overrides` | 连续 goal_set 两次 → 旧 goal=Cancelled |
| `test_guard_max_iterations` | iteration_count 到顶 → Exhausted |
| `test_guard_max_duration` | 模拟超时 → Exhausted |
| `test_repetition_detection` | 相似 actionPlan → repetitive=true |
| `test_log_schema_complete` | iterations.jsonl 含全部字段 |

### 4.2 Harness 脚本（`tests/goal_supervisor_ci.sh`）

**场景 A1**：set → 全 PASS → complete
```bash
# FauxProvider: 第 1 轮 agent 调 bash 写文件
# goal_set 带 checks=[test -f /tmp/goal_test_file]
# 期望：iterations.jsonl 1 条 + final-report outcome=complete
```

**场景 A2**：set → FAIL → continue → PASS → complete
```bash
# FauxProvider Factory: 第 1 轮不写文件，第 2 轮（收到 continue）写
# 期望：iterations.jsonl 2 条，iter1 FAIL iter2 PASS，final complete
```

**场景 B1**：max_iter 到顶 → exhausted
```bash
# FauxProvider: 每轮都不写文件，永远 FAIL
# max_iterations=3
# 期望：跑满 3 轮后停止，final exhausted
```

脚本格式参照 `tests/file_snapshot_ci.sh`。

---

## 5. coordinator 拆 task 建议

B1 虽然是"一个模块"，但建议 coordinator 拆成 2-3 个串行子任务（降低单次 B session 复杂度）：

| 子任务 | developer prompt 要点 | 验收 |
|--------|---------------------|------|
| **B1-a：数据结构 + 工具骨架** | 实现 Check/CheckResult/Evidence/GoalState/Config + goal_set tool（execute 先返回固定值）+ 注册 + 单元测试（数据结构） | cargo build + 单测过 |
| **B1-b：run_all_checks + 证据收集** | 实现 run_single_check（tokio::process::Command 执行 + 按 pass_criteria 判定 + evidence 落盘）+ 单测（各种 criteria） | 单测过 |
| **B1-c：状态机 + 防线 + 闭环 + 日志** | on_agent_end 串联 run_checks/guards/inject_continue/log + harness 脚本 | harness A1/A2/B1 过 |

每个子任务 A 守门通过后再进下一个。

---

## 6. 验收（A 合并回主仓库前）

```bash
# 在主仓库跑
cargo build                                    # 通过
cargo test --lib goal_supervisor               # 全过
cargo clippy -- -D warnings                    # 无新增
grep -rc $'\xef\xbf\xbd' src/ | grep -v ':0$'  # 空（无 U+FFFD）
bash tests/goal_supervisor_ci.sh               # A1+A2+B1 过
```

全过 → A 合并 → commit `feat(goal): Goal Supervisor B1 — core loop + checks + guards`。

---

## 7. 风险与注意事项

| 风险 | 应对 |
|------|------|
| B 不熟悉 ExtensionApi | spec 里给了骨架，B read 现有 extension（如 learning_extension.rs）参考 |
| inject_continue 机制不清 | B1 先用最简单方式：ctx 注入 user message（参考 hooks prompt handler） |
| 检测命令在 container 里执行环境不同 | B1 检测项用简单命令（test -f / echo），不依赖复杂环境 |
| 文件大编译慢 | B 只跑 `cargo check`（不 build），A 在主仓库跑完整 build+test |
| U+FFFD | comment 全英文（铁律） |
