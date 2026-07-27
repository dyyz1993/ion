# Goal Supervisor Fixture 数据集 — 10 个复杂场景

> **用途**：给 goal-evolver agent 的分析输入。每个场景模拟真实 goal 运行的日志产物（iterations.jsonl + final-report.json），覆盖 goal 闭环可能遇到的各种问题。
>
> **目标**：让 goal-evolver 能从中分析出死循环 / 模型选择错误 / 上下文缺失等模式，从而提 Issue 改进 skill 和 config。

## 场景矩阵

| # | 目录 | 场景 | 预期 evolver 动作 | 覆盖维度 |
|---|------|------|------------------|---------|
| 01 | `case_01_healthy` | 健康完成（一次过） | **不提 Issue**（baseline） | 健康 |
| 02 | `case_02_deadloop_strict_check` | 死循环：检测项太严（agent 缺 Cargo.toml 步骤） | 提 Issue：skill 补"async 导入需改 Cargo.toml" | Q1 死循环 |
| 03 | `case_03_deadloop_weak_agent` | 死循环：agent 能力不足（Send/Sync 反复失败） | 提 Issue：换 max tier 模型 + skill 补并发模式 | Q1 死循环 |
| 04 | `case_04_model_wrong_for_checks` | 模型错：generate_checks 用弱模型（漏安全检测） | 提 Issue：generate_checks 换 max tier | Q2 模型 |
| 05 | `case_05_model_wrong_for_analysis` | 模型错：analyze_failure 用弱模型（分析无效） | 提 Issue：analyze_failure 换 max tier | Q2 模型 |
| 06 | `case_06_missing_context_tests` | 上下文缺：测试结果没给（confidence 低） | 提 Issue：skill 注入 test stdout | Q3 上下文 |
| 07 | `case_07_missing_context_diff` | 上下文缺：git diff 没给（检测项遗漏） | 提 Issue：context 必须含 diff | Q3 上下文 |
| 08 | `case_08_cost_explosion` | 成本爆炸（$5.20 / 3 轮） | 提 Issue：大目标拆分 + cost 限制合理 | 边界 |
| 09 | `case_09_duration_explosion` | 时长爆炸（每轮 5 分钟编译） | 提 Issue：perf 目标不适合闭环 | 边界 |
| 10 | `case_10_hard_won_success` | 成功但曲折（repetitive 后换角度成功） | **不提 Issue**（repetitive guard 工作正常） | 健康（验证换策略） |

## 数据结构

每个场景目录包含：

```
case_XX_<name>/
  ├── iterations.jsonl    # 每行一个 iteration（核心日志）
  └── final-report.json   # 最终结果（含 outcome 回填）
```

### iterations.jsonl 字段（对齐 GOAL_SUPERVISOR.md §7.2）

| 字段 | 类型 | 说明 |
|------|------|------|
| `iter` | number | 迭代号（1-based） |
| `timestamp` | string | "epoch:NNN" 格式 |
| `session_id` / `goal_id` | string | 标识 |
| `objective` | string | 目标描述 |
| `guards_hit` | object | 各防线是否触发 |
| `similarity_to_prev` | number/null | 跟上轮 action plan 的相似度 |
| `llm_calls` | array | LLM 调用记录（purpose/model/质量） |
| `context_snapshot` | object | 上下文快照（消息数/文件/diff/测试/skill） |
| `checks_run` | array | 检测结果（含 evidence） |
| `all_passed` / `failed_checks` | - | 汇总 |
| `total_elapsed_ms` / `total_cost_usd` | - | 累计 |

### final-report.json 字段

| 字段 | 说明 |
|------|------|
| `final_status` | complete / exhausted / blocked |
| `stopped_reason` | all_checks_passed / max_iterations / repetitive / max_cost / max_duration |
| `guards_hit_summary` | 各防线触发次数 |
| `outcome` | fixed / abandoned |
| `outcome_detail.diagnosis_hint` | **给 evolver 的诊断提示**（分析方向） |

## evolver 分析维度对照

### Q1：死循环风险
- **case_02**：repetitive + exhausted，同检测项反复 FAIL（skill 缺步骤）
- **case_03**：不同检测项交替 FAIL（agent 能力不足，需换模型）
- **case_10**：repetitive 触发后成功（换策略有效，**不提 Issue**）

### Q2：模型选择
- **case_04**：generate_checks 用 deepseek-v4-flash → 检测项漏（质量差）
- **case_05**：analyze_failure 用 deepseek-v4-flash → analysis_used=false（无效）

### Q3：上下文充分性
- **case_06**：test_results_included=false → confidence 低（缺测试输出）
- **case_07**：git_diff_lines=0 → 检测项遗漏（缺 diff）

### 边界场景
- **case_08**：cost 合理触发（大目标，skill 应建议拆分）
- **case_09**：duration 合理触发（perf 目标不适合闭环）

### 健康场景（不应提 Issue）
- **case_01**：一次过
- **case_10**：曲折但成功（repetitive guard 正常工作）

## 使用方法

```bash
# dry_run：分析但不真提 Issue（CI 用）
ion rpc --method goal_evolver_run_once \
  --params '{"data_dir": "tests/fixtures/goal-runs/case_02_deadloop_strict_check/", "dry_run": true}'

# 分析全部场景
ion rpc --method goal_evolver_run_once \
  --params '{"data_dir": "tests/fixtures/goal-runs/", "dry_run": true}'

# 期望输出（case_02）：
# {
#   "analyzed_goals": 1,
#   "issues_planned": [{
#     "title": "[goal-evolver] deadloop: skill missing Cargo.toml step for async imports",
#     "dimension": "deadloop",
#     "evidence": {"goal_id": "goal_strict_check", "iters": [1,2,3]},
#     "suggestion": "Add 'update Cargo.toml when adding async crates' to rust-ci skill"
#   }]
# }
```

## 设计原则

1. **每个场景有明确的 `diagnosis_hint`** — 告诉 evolver 该发现什么
2. **健康场景（01/10）不应触发 Issue** — 测试 evolver 不误报
3. **数据真实** — 模拟真实 goal 运行的迭代次数、检测项、证据格式
4. **覆盖完整** — Q1/Q2/Q3 + 边界 + 健康，5 个维度全覆盖
