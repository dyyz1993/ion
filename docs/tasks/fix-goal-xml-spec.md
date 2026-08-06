# Task Spec: GoalSupervisor RetryWith 消息改 XML 格式

> **状态：待 B 执行** | 改动范围：`src/goal_supervisor_extension.rs` | 单函数改动

## 背景

GoalSupervisor 的 `on_gate_check` 在检测项没全 PASS 时返回 `GateDecision::RetryWith(msg)`，把 `msg` 当 user message 注入对话。

当前 `msg` 是**纯文本 + emoji + markdown**，不符合项目的自动注入规范（其他扩展如 memory/hooks/rules/diagnostics 都用 XML 包裹）。

## 修法

把 `src/goal_supervisor_extension.rs` 的 `inject_continue` 函数（约 1285-1343 行）里构造 `msg` 的逻辑改成：**外层一个 `<goal_feedback>` XML 标签，里面保持自然语言描述**。

### 当前代码（要改的部分）

文件 `src/goal_supervisor_extension.rs`，`inject_continue` 方法，大约 1290-1343 行：

```rust
let mut msg = String::from("Goal not complete. The following checks failed:\n");
for r in &failed {
    let evidence_excerpt = r.evidence.as_ref()
        .and_then(|e| e.stdout_excerpt.as_deref())
        .unwrap_or("(no evidence)");
    msg.push_str(&format!("- {} (evidence: {})\n", r.name, evidence_excerpt));
}
msg.push_str("Fix the failing checks before stopping.");

// ... remaining steps + acceptance criteria + progress + repetitive nudge ...
```

### 改成

外层用 `<goal_feedback>` 包裹，内部保持人类可读的自然语言（不嵌套子 XML 标签）：

```rust
let mut body = String::new();

// Failed checks section
body.push_str("Goal not complete. The following checks failed:\n");
for r in &failed {
    let evidence_excerpt = r.evidence.as_ref()
        .and_then(|e| e.stdout_excerpt.as_deref())
        .unwrap_or("(no evidence)");
    body.push_str(&format!("- {} (evidence: {})\n", r.name, evidence_excerpt));
}
body.push_str("Fix the failing checks before stopping.");

// Remaining steps
{
    let guard = self.state.lock().ok();
    if let Some(state) = guard.as_ref().and_then(|g| g.as_ref()) {
        let pending = state.goal_plan.pending_step_descriptions();
        if !pending.is_empty() {
            body.push_str(&format!("\nRemaining steps ({}):\n", pending.len()));
            for p in &pending {
                body.push_str(&format!("  - {}\n", p));
            }
        }
        if !state.goal_plan.acceptance_criteria.is_empty() {
            body.push_str("\nAcceptance criteria:\n");
            for c in &state.goal_plan.acceptance_criteria {
                body.push_str(&format!("  - {}\n", c));
            }
        }
    }
}

// Progress analysis
let progress = self.analyze_progress(current_plan.as_deref());
body.push_str(&format!("\nProgress: {:?}. {}", progress.trend, progress.recommendation));

// Repetition nudge
if current_plan.as_deref()
    .map(|p| self.check_guards(Some(p)).as_deref() == Some("repetitive"))
    .unwrap_or(false)
{
    body.push_str(" NOTE: previous attempt was similar. Try a different approach.");
}

// Wrap in single outer XML tag
let msg = format!("<goal_feedback>\n{}\n</goal_feedback>", body);
```

### 关键要求

1. **只改 msg 的构造方式**——用 `<goal_feedback>...</goal_feedback>` 包裹原来的内容
2. **去掉 emoji**（📊📋✅）——XML 内容里不要用 emoji，保持纯文本
3. **不嵌套子 XML 标签**——里面就是自然语言 + markdown 列表（`- item`）
4. **不改变 GateDecision::RetryWith 的返回**——还是返回 `Ok(GateDecision::RetryWith(msg))`
5. **不改变其他逻辑**——check 执行、guard 检查、tracing 日志都不变

### 不要改的

- `on_gate_check` 的其他部分（check 执行、guard、状态更新）
- `GateDecision` 枚举定义
- 测试里的 assert（测试检查 `msg.contains("Goal not complete")` 仍然成立，因为内容还在）

## 验证

```bash
# 1. 编译
cargo check 2>&1 | tail -3

# 2. 单元测试（goal_supervisor 的测试检查 msg 内容）
cargo test --lib goal_supervisor 2>&1 | tail -10

# 3. 确认 msg 格式（如果有测试检查格式的话）
# 测试里 assert msg.contains("Goal not complete") 应该仍然通过
# 如果有测试检查不含 XML 标签的，需要更新断言
```

## 守门

- ✅ `cargo check` 无错误
- ✅ `cargo test --lib goal_supervisor` 全过
- ✅ 无 U+FFFD（英文 comment）
- ✅ 不改 Cargo.toml
