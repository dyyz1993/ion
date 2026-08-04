# 扩展验证报告（EXT-02 ~ EXT-06）

> **状态：定稿**（2026-08-05）
> **方法**：`scripts/validate_extension.sh` + 9 项硬性指标 + 人工核查扩展真实触发
> **结论**：**5/5 全过**，并发现/修复 3 个阻断性 bug

---

## 1. 结果总览

| ID | 扩展 | 9 项指标 | 扩展真实触发 | 关键证据 |
|---|---|---|---|---|
| EXT-02 | GlobalMemoryExtension | ✅ 9/9 | ✅ 工具被调用 | `memory_save` + `memory_search` 各 1 次，返回 `gmem_xxx` ID |
| EXT-03 | DevServerDetectorExtension | ✅ 9/9 | ✅ bash 触发 | `python3 -m http.server 8765` 后台运行 |
| EXT-04 | FileSnapshotExtension | ✅ 9/9 | ✅ snapshot 记录 | HTML 里 "snapshot" 出现 21 次 |
| EXT-05 | LspExtension | ✅ 9/9 | ✅ lsp_check 调用 | `lsp_check` 工具被调用 8 次 |
| EXT-06 | HookExtension | ✅ 9/9 | ✅ hook 真实执行 | `hook_log.txt` 有 `[HOOK-TRIGGERED]` + 2× `[HOOK-STOP]` |

HTML 文件位置：`/tmp/ext_validation_reports/EXT-{0X}_{Name}.html`

---

## 2. 验证过程发现的 3 个 bug（已修复）

### Bug 1: `default_ci_checks` pipeline 吞掉 cargo 退出码（已修复）

**之前**：`cargo build --lib 2>&1 | tail -1` → 永远返回 0（tail 的退出码），即使 cargo 失败也"通过"。
**修复**：`set -o pipefail; cargo build --lib 2>&1 | tail -5`，让 pipeline 返回 cargo 的真实退出码。
**Commit**：`1875661`

### Bug 2: cmd_run 没注册 memory / lsp_check 工具（已修复）

**之前**：`build_tools()` 只注册 BashExtension 的工具，GlobalMemoryExtension（singleton RPC）和 LspExtension 都没暴露 LLM 工具给 cmd_run 路径。
**后果**：EXT-02 / EXT-05 之前测试都是空跑——LLM 调不到 `memory_save` / `lsp_check`。
**修复**：在 cmd_run 的扩展注册区补：
- `MemorySaveTool` + `MemorySearchTool`（共享 `MemoryStore`，对齐 worker_rpc:367-375）
- `LspExtension` 注册到 `ext_reg` + `LspCheckTool` 注册到 agent（共享 diagnostics handles，对齐 worker_rpc:967-979 + 1120-1128）
**Commit**：`54d31c2`

### Bug 3: session_name custom entry 重复 + 强制顶部 + 类型不匹配（已修复）

**之前 3 个问题**：
1. `export.rs:630` 找 `type == "session_name"`，但 cmd_run 写入的是 `type="custom_message"` + `customType="session_name"` → 找不到，HTML `<title>` fallback 到首条 user message
2. cmd_run 强制把 session_name 插到对话顶部（`parentId=header_id` + 改写第一条 message 的 parentId），违反时间顺序
3. 同一个 session 多次进入插入逻辑会重复追加

**修复**：
- 撤销"强制顶部"，改成按时间顺序 append（`parentId=最后一条entry的id`）
- 去重：已存在 session_name entry 就更新内容 + timestamp，不再追加
- export.rs 兼容两种格式（legacy `type=session_name` 和 new `type=custom_message` + `customType=session_name`）
- CSS 加 `.session-name-card` 青色渐变背景，JS 后处理扫描 `[session_name]` 文本给卡片加 class

**Commit**：`61a81e1`

---

## 3. 前置配置变更

`~/.ion/config.json` 改了两项（之前默认禁用，导致 EXT-02/04 空跑）：

```json
"extensions": {
  "file-snapshot":   { "enabled": true },   // 之前 false
  "global-memory":   { "enabled": true },   // 之前 false
  "lsp":             { "enabled": true },
  "dev_server_detector": { "enabled": true }
}
```

EXT-06 测试目录 `/tmp/ext_validate_EXT-06/.ion/hooks.json` 预先配置了 PostToolUse + Stop hook（command 类型，写日志文件）。

---

## 4. 验证方法论（严谨性体现）

不只看 9 项硬性指标，每个扩展额外**人工核查扩展真的被触发**：

| 扩展 | 人工核查方法 | 证据 |
|---|---|---|
| EXT-02 | 检查 session.jsonl 有无 memory_save/search 工具调用 | ✅ 工具被调用，返回 `gmem_xxx` ID |
| EXT-03 | 检查 HTML 里 `8765` / `python3 -m http.server` | ✅ bash 后台启动了 http server |
| EXT-04 | 检查 HTML 里 snapshot/checkpoint 关键字 | ✅ snapshot 出现 21 次 |
| EXT-05 | 检查 HTML 里 `lsp_check` 工具调用次数 | ✅ 被调用 8 次 |
| EXT-06 | 检查 hook 命令的输出文件 | ✅ `hook_log.txt` 有触发记录 |

---

## 5. 任务编号统一

`scripts/extension_tasks.sh` 和 plan 文档之前对不上（DevServerDetector 和 FileSnapshot 顺序反了）。已统一以脚本为准：

| ID | 扩展 |
|---|---|
| EXT-01 | BashExtension |
| EXT-02 | GlobalMemoryExtension |
| EXT-03 | DevServerDetectorExtension |
| EXT-04 | FileSnapshotExtension |
| EXT-05 | LspExtension |
| EXT-06 | HookExtension |

---

## 6. 相关 commits（按时间倒序）

| Commit | 说明 |
|---|---|
| `61a81e1` | session_name 按时间顺序 + 去重 + 青色背景 |
| `54d31c2` | cmd_run 补注册 memory + lsp_check 工具 |
| `354574f` | GOAL_vs_CLAUDE_CODE.md 对比文档 |
| `cc53360` | GoalSupervisor wiring 测试 |
| `e1dae6a` | on_gate_check 4 个单元测试 |
| `1875661` | default_ci_checks pipefail 修复（CRITICAL） |

---

## 7. 后续建议

- **EXT-01 重跑**：BashExtension 之前未跑 9 项硬性校验，建议用新脚本重跑确认。
- **`validate_extension.sh` 改进**：当前 Step 2 用"最新 session_dir"导出，多个测试同目录时会拿错（本次 EXT-02 触发）。建议改成 `--name` 强制每次新目录，或读 turn_summary 里最新 sess_id。
- **EXT-04 / EXT-06 自动化**：FileSnapshot 和 Hook 测试目前需要人工准备 `.ion/hooks.json` / 文件系统初始状态。可以把准备工作写进 `extension_tasks.sh` 的 `pre_setup()` 钩子。
