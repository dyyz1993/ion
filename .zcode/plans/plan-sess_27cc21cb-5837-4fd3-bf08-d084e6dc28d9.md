# Plan: 复杂端到端验证 — LSP Fix-Suggestion 提取 + 多智能体编排

## 目标

用一个真实的功能开发任务，验证 ION 的**完整能力链**：多智能体编排 + worktree 隔离 + 真实 LLM 改代码 + 审查 + 合并 + 测试验证。

## 任务

给 `src/lsp_extension.rs` 加 **fix-suggestion 提取**功能：
- 扩展 `Diagnostic` 结构体加 `suggestion: String` 字段
- 新增 `extract_fix_suggestion()` 方法从 cargo/clippy JSON 提取建议
- 在 `parse_cargo_check_json` 里调用
- 在 XML/text 格式化里展示
- 加 3 个单元测试

**只改 1 个文件**（src/lsp_extension.rs），~50-80 行新代码。

## 验证覆盖的功能

| 功能 | 怎么覆盖 |
|------|---------|
| **多智能体编排** | coordinator → developer (worktree) → reviewer → merger |
| **worktree 隔离** | developer 在独立 git 分支改代码 |
| **真实 LLM** | GLM-5.2 真的写 Rust 代码 |
| **Session 管理** | 每个 worker 独立 session |
| **File Snapshot** | 代码改动被追踪 |
| **事件流** | subscribe 看 text_delta + agent_start/end |
| **权限** | developer 有 write/edit/bash，reviewer 只有 read |
| **Hooks** | LSP extension 的 on_tool_execution_end 自动触发 |

## 执行方式

### 方式 A（推荐）：直接用 `ion --host --agent coordinator`

```bash
echo "任务 spec" | ./target/debug/ion --host --agent coordinator \
    --provider zai --model glm-5.2
```

coordinator 自动：
1. spawn_worker(developer, worktree=true, wait=true) — 在隔离分支改代码
2. spawn_worker(reviewer, wait=true) — 审查 + U+FFFD 检查
3. 如果 reviewer REQUEST_CHANGES → resume_worker(developer) 修
4. spawn_worker(merger, wait=true) — git merge 回主分支
5. bash: cargo test --lib lsp_extension — 验证全过

### 方式 B（fallback）：A→B container 模式

如果方式 A 的 LLM coordinator 不稳定（之前遇到过），用 evolve_self.sh 模式。

## 验收标准

- ✅ `cargo test --lib lsp_extension` 全过（原 13 + 新 3 = 16 个测试）
- ✅ `grep -c U+FFFD src/lsp_extension.rs` == 0（无中文乱码）
- ✅ `cargo build --bin ion` 编译通过
- ✅ coordinator 真的 spawn 了 ≥2 个 worker（developer + reviewer）
- ✅ 最终代码改动在主分支上（不是只留在 worktree）

## 不做的事

- 不改 agent_loop.rs（避免 blast radius）
- 不加新文件（只改 src/lsp_extension.rs）
- 不动其他 CI 脚本
- 如果 coordinator LLM 挂了，切方式 B，不强求 A