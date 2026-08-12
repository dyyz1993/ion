# ION 全模块验收大纲

> **目的**：逐模块看清"有什么功能 → 有什么测试 → 每步验证什么 → 状态如何"
>
> **用法**：从上到下扫一遍，挑一个模块开始验收，一个一个过。

---

## 模块总览（按功能域分组）

### 🅰 文件与代码

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 1 | **FileSnapshot** | 双路快照 + 审批 + 回滚 | `file_snapshot_ci.sh` (14 组) + `ext04_serve_ci.sh` (**复杂场景**) + `file_snapshot_e2e.sh` + `rollback_impact_ci.sh` | 14+18+5+1 | ✅ 已验证 |
| 2 | **LSP Extension** | 多语言诊断 (Rust/TS/Python/Go/HTML) | `lsp_ci.sh` (12 组) | 12 | ⚠️ 待验收 |
| 3 | **Bash** | 同步执行 + 后台进程 | `abort_ci.sh` (6 组) | 6 | ⚠️ 待验收 |

### 🅱 记忆与学习

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 4 | **GlobalMemory** | 跨项目 SQLite+FTS5 记忆库 | `global_memory_ci.sh` (3 组) + `memory_agent_ci.sh` (6 组) + `memory_active_ci.sh` (12 组) + `memory_v2_processing_ci.sh` (6 组) + `ext02_serve_ci.sh` (**复杂场景**) | 3+6+12+6+18 | ✅ 已验证 |
| 5 | **Memory V0.1** | 项目级 outline 索引记忆 | `memory_injection_ci.sh` | 2 | ⚠️ 待验收 |
| 6 | **Learning Extension** | Session 分析 + 密钥脱敏 | `learning_e2e.sh` | — | ⚠️ 待验收 |
| 7 | **Skill Distillation** | LLM 提炼技能 | `skill_tool_ci.sh` (9 组) | 9 | ⚠️ 待验收 |
| 8 | **Secret Detector** | API key/token 检测 | (集成在 Learning 中) | — | ⚠️ 待验收 |

### 🅲 安全与权限

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 9 | **Permission** | 5 种权限模式 + 路径权限 | `permission_ci.sh` (11 阶段) + `permission_store_ci.sh` (3 组) | 11+4+3 | ⚠️ 待验收 |
| 10 | **CommandGuard** | 危险命令拦截 | (集成在 permission_ci.sh) | — | ⚠️ 待验收 |

### 🅳 编排与工作流

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 11 | **Hooks** | 5 种 handler + 热更新 | `hooks_ci.sh` (12 组) + `hooks_handler_ci.sh` (5 组) + `hooks_stdin_ci.sh` (10 组) + `hooks_pretool_deny_ci.sh` (8 组) + `ext06_serve_ci.sh` (**复杂场景**) + 4 more | 12+5+10+8+13+4 | ✅ 已验证 |
| 12 | **GoalSupervisor** | 证据驱动目标闭环 (on_gate_check) | `goal_supervisor_ci.sh` (8 组) + `goal_evolver_ci.sh` (9 组) + `ext07_serve_ci.sh` (**复杂场景**) + 2 e2e | 8+9+13+2 | ✅ 已验证 |
| 13 | **Workflow Engine** | 结构化交付流水线 DSL | `workflow_ci.sh` (21 组) | 21 | ⚠️ 待验收 |
| 14 | **Plan** | plan_enter/exit/add/list/done/approve | (plugin_tests.rs) | — | ⚠️ 待验收 |
| 15 | **Monitor** | 定时监控→触发 LLM 对话 | `monitor_ci.sh` (24 组) + `self_heal_ci.sh` (10 组) | 24+10 | ⚠️ 待验收 |

### 🅴 会话与消息

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 16 | **SessionTree** | 文件内分支 + 回滚 | `session_tree_ci.sh` (4 组) + `session_tree_verify.sh` | 4 | ⚠️ 待验收 |
| 17 | **MessageRetrieval** | 9 接口拉取/分页/过滤 | `message_retrieval_ci.sh` (37 组) | 37 | ⚠️ 待验收 |
| 18 | **Compaction** | 会话压缩 + LLM summarizer | `compaction_ci.sh` (9 组) | 9 | ⚠️ 待验收 |
| 19 | **SoftDelete** | 软删除/恢复 | `soft_delete_ci.sh` (5 阶段) | 5 | ⚠️ 待验收 |

### 🅵 外部集成

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 20 | **MCP** | rmcp 客户端 + 工具发现 | `mcp_ci.sh` (16 组) + `mcp_agent_tools_ci.sh` (13 组) | 16+13 | ⚠️ 待验收 |
| 21 | **Record/Replay** | LLM 录制回放 | `record_replay_ci.sh` (3 组) + `streaming_replay_ci.sh` (7 组) | 3+7 | ⚠️ 待验收 |
| 22 | **DevServerDetector** | bash 启动 dev server 端口检测 | `dev_server_detector_ci.sh` (9 组) | 9 | ⚠️ 待验收 |

### 🅶 核心引擎

| # | 模块 | 功能 | 测试脚本 | 测试步骤数 | 状态 |
|---|------|------|---------|-----------|------|
| 23 | **RPC 协议** | JSONL over stdin/stdout | `cli_alignment_ci.sh` (16 组) + `unit_rpc_test.rs` (25 组) | 16+25 | ⚠️ 待验收 |
| 24 | **HTML Export** | 单文件离线导出 | `export_ci.sh` (54/54) | 54 | ✅ 已验证 |
| 25 | **Runtime/Backend** | Local/Sandbox/Remote/Container | `runtime_ci.sh` (8 组) + `apple_container_ci.sh` (19 组) | 8+19 | ⚠️ 待验收 |
| 26 | **WASM Extension** | 热更新运行时扩展 | `extensions_ci.sh` + `extension_cli_ci.sh` (8 组) + `extension_fs_ci.sh` (9 组) | 8+9 | ⚠️ 待验收 |
| 27 | **UI EventBus** | 多终端实时同步 | `ui_integration_ci.sh` (17 阶段) + `sse_events_ci.sh` (6 组) | 17+6 | ⚠️ 待验收 |

---

## 已验证模块的详细步骤

### EXT-04 FileSnapshot（18/0 ✅）

```
Phase 0: cargo build + git 项目 + .ion/settings.json (file-snapshot:enabled)
Phase 1: 启动 serve (zai/glm-5.2, skip MCP)
Phase 2: create_session (model=glm-5.2, provider=zai)
    ↓
Phase 3: LLM 用 write 创建 Rust 计算器项目
    Prompt: "创建 Cargo.toml + src/main.rs，实现 add/sub/mul/div 计算器"
    ✅ 验证: Cargo.toml 含 "calc" + src/main.rs 含 "fn main"
    ↓
Phase 4: review_pending + 部分审批
    ✅ V1 review_pending: 2 个 pending (Cargo.toml + src/main.rs)
    ✅ V2 review_approve Cargo.toml: approved (锚定 baseline)
    ✅ 部分审批验证: src/main.rs 仍在 pending
    ↓
Phase 5: LLM 用 write 修改 src/main.rs（添加 mod 功能）
    Prompt: "给计算器添加取模运算 (mod) 支持"
    ✅ 验证: src/main.rs 含 mod/取模 代码
    ↓
Phase 6: re-approval + review_reject（回滚 mod 功能）
    ✅ L2 re-approval: src/main.rs 自动回到 pending
    ✅ V3 review_reject: action=deleted/restored, rolledBack=yes
    ↓
Phase 6b: LLM 验证回滚（HTML 可见）
    Prompt: "用 read 读 src/main.rs + cargo run -- mod 验证 mod 是否被回滚"
    ✅ HTML 可见: read tool result + LLM 确认回滚
    ↓
Phase 7: LLM 用 bash 编译验证
    Prompt: "执行 cargo build + cargo run -- add 10 20 + cargo run -- sub 15 5"
    ✅ HTML 可见: bash output (编译成功 + 运算结果)
    ↓
Phase 8: get_modified_files + get_file_diff
Phase 9: 导出 HTML (kill serve → 等文件稳定 → export)
```

### EXT-02 GlobalMemory（18/0 ✅）

```
Phase 0-2: build + serve + create_session
    ↓
Phase 3: 确定性 save 3 条丰富记忆
    ✅ save 架构决策: "parking_lot::Mutex 替代 tokio::sync::Mutex 解决死锁"
    ✅ save bugfix: "Rust UTF-8 切片 panic，用 chars().take(N) 替代"
    ✅ save 性能模式: "FauxProvider Factory 闭包，零成本测试"
    ↓
Phase 4: 多角度 search
    ✅ 英文 FTS5: query="deadlock mutex" → 找到架构决策
    ✅ 中文 bigram: query="死锁 异步" → 找到架构决策
    ✅ 语义匹配: query="testing mock" → 找到性能模式
    ✅ list: 3 条记忆
    ↓
Phase 5: LLM 自主保存观察 (global_memory_save 工具)
    Prompt: "保存你对测试项目的理解"
    ✅ 验证: search 找到 LLM 保存的记忆
    ↓
Phase 6: LLM 搜索总结 (global_memory_search 工具)
    Prompt: "搜索 deadlock/mutex 记忆，总结 parking_lot 解决了什么问题"
    ↓
Phase 7: forget + bash 验证
    ✅ forget: gmem_xxx ok (软删除)
    ✅ list 验证: 该条已消失
    ↓
Phase 8: 导出 HTML + 清理测试 memory
```

### EXT-06 Hooks（13/0 ✅）

```
Phase 0: 配置 hooks.json（3 类 Hook，inline 命令）
    SessionStart → echo "SessionStart-fired" >> hook.log
    PreToolUse(bash) → grep "rm -rf" → exit 2 (block)
    PostToolUse(write) → echo "PostToolUse-write" >> hook.log
    PostToolUse(bash) → echo "PostToolUse-bash" >> hook.log
    ↓
Phase 1-2: serve + create_session
    ↓
Phase 3: SessionStart hook 验证（重试等待 worker 启动）
    ✅ hook.log 有 "SessionStart-fired"
    ↓
Phase 4: LLM 尝试 rm -rf → PreToolUse 拦截
    Prompt: "用 bash 执行 rm -rf /tmp/xxx"
    ✅ hook.log 有 "PreToolUse-BLOCKED-rm-rf" (exit 2)
    (或 soft-pass: LLM 安全训练拒绝执行 rm -rf)
    ↓
Phase 5: LLM 用 write 创建文件 → PostToolUse 审计
    Prompt: "用 write 创建 config.json"
    ✅ hook.log 有 "PostToolUse-write-fired"
    ↓
Phase 6: LLM bash + read hook 日志验证（HTML 可见）
    Prompt: "用 bash echo safe-ok + read hook 日志总结哪些 hook 触发了"
    ✅ HTML 可见: read tool result (SessionStart/PreToolUse/PostToolUse 各几次)
    ↓
Phase 7: 导出 HTML
```

### EXT-07 GoalSupervisor（13/0 ✅）

```
Phase 0-2: build + serve + create_session
    ↓
Phase 3: goal_evolver_run_once 分析 fixture（确定性）
    ✅ evolver(healthy fixture): 分析成功
    ✅ evolver(deadloop fixture): 分析成功
    ↓
Phase 4+5: LLM goal_set + write sort.py（冒泡排序）
    Prompt: "1. 用 goal_set 设定目标 'create bubble sort script'
             checks: [{script_exists: test -f sort.py}, {script_runs: python3 sort.py}]
             2. 用 write 创建 sort.py，实现冒泡排序"
    ✅ sort.py 已创建 + 含排序逻辑
    ↓
Phase 6: on_gate_check 验证
    ✅ goal-runs/ 目录存在 (on_gate_check 已触发)
    ✅ iterations.jsonl: CI checks 已执行
    ✅ final-report.json: status=complete
    ↓
Phase 6b: LLM 验证目标完成（HTML 可见）
    Prompt: "用 read 读 sort.py + bash python3 sort.py 确认输出正确"
    ✅ HTML 可见: read + bash tool result (排序结果 [11, 12, 22, 25, 34, 64, 90])
    ↓
Phase 7: 导出 HTML
```

---

## 待验收模块（按优先级排序）

### 优先验收（核心功能，高频使用）

| 优先级 | 模块 | 测试脚本 | 建议验收方式 |
|--------|------|---------|-------------|
| **P0** | Permission | `permission_ci.sh` (11 阶段) | 跑 CI + 导出 HTML |
| **P0** | SessionTree | `session_tree_ci.sh` + `session_tree_verify.sh` | 跑 CI + 验证分支/回滚 |
| **P0** | Compaction | `compaction_ci.sh` (9 组) | 跑 CI + 验证压缩前后消息 |
| **P0** | MCP | `mcp_ci.sh` (16 组) | 跑 CI + 验证工具发现 |
| **P1** | Workflow | `workflow_ci.sh` (21 组) | 跑 CI + 验证 DSL 执行 |
| **P1** | LSP | `lsp_ci.sh` (12 组) | 跑 CI + 验证诊断注入 |
| **P1** | MessageRetrieval | `message_retrieval_ci.sh` (37 组) | 跑 CI + 验证分页/过滤 |
| **P1** | Monitor | `monitor_ci.sh` (24 组) | 跑 CI + 验证定时触发 |

### 次优先验收（辅助功能）

| 优先级 | 模块 | 测试脚本 |
|--------|------|---------|
| P2 | WASM Extension | `extensions_ci.sh` + `extension_fs_ci.sh` |
| P2 | Record/Replay | `record_replay_ci.sh` |
| P2 | DevServerDetector | `dev_server_detector_ci.sh` |
| P2 | UI EventBus | `ui_integration_ci.sh` |
| P2 | Runtime/Backend | `runtime_ci.sh` |
| P3 | Learning/Skill | `learning_e2e.sh` + `skill_tool_ci.sh` |
| P3 | SoftDelete | `soft_delete_ci.sh` |
| P3 | Plan | `plugin_tests.rs` |
| P3 | Rules Engine | `rules_ci.sh` |

---

## 验收流程模板（每个模块统一执行）

```
1. 跑 CI 脚本：bash tests/<module>_ci.sh
2. 检查 PASS/FAIL 数量
3. 导出 HTML（脚本内自动导出，或手动 ion --export）
4. 打开 HTML，检查：
   a. Flow Summary（LLM calls / tool calls / custom entries）
   b. Timeline（用户 prompt → LLM 回复 → tool call → tool result）
   c. 关键验证点是否可见（如 approval_deny XML、hook 触发记录、goal check 结果）
5. 记录验收结果到本文档
```
