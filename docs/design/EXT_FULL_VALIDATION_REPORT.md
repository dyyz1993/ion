# 扩展完整验证报告 v2.1（15 场景全跑分 + 5 bug 修复）

> **状态**：定稿（2026-08-05）
> **范围**：5 扩展 × 3 场景 = 15 场景，全部用 v2 基础设施跑完
> **结论**：**14/15 PASS，发现并修复 5 个真实 bug**

---

## 1. 最终跑分

| 场景 | 状态 | pass/total | 关键发现 |
|---|---|---|---|
| **EXT-02 GlobalMemory** ||||
| 02-S1 save+search | ✅ PASS | 16/17 | gmem_id round-trip 正常；02-M5 warn（路径已修） |
| 02-S2 空查询 | ✅ PASS | 13/17 | 正确处理无结果情况 |
| 02-S3 tags 多关键字 | ✅ PASS | **17/17** | 完美 |
| **EXT-03 DevServerDetector** ||||
| 03-S1 Python http | ✅ PASS | 15/15 | bash bg + 8765 端口 + PID 36734 |
| 03-S2 不同端口 | ✅ PASS（重跑） | 14/15 | **首次因 UTF-8 panic 崩溃 → 修复后 PASS** |
| 03-S3 多 server 并发 | ✅ PASS | 14/15 | 2 个 bash bg + 2 个 PID |
| **EXT-04 FileSnapshot** ||||
| 04-S1 单文件 | ✅ PASS | 16/16 | write 调用 + snapshot 落盘 |
| 04-S2 多文件 | ✅ PASS | 16/16 | 3 次 write + 多 snapshot |
| 04-S3 编辑后 diff | ✅ PASS | 16/16 | write × 2 + diff 展示 |
| **EXT-05 Lsp** ||||
| 05-S1 干净代码 | ❌ FAIL | 15/17 | **LLM 没主动调 lsp_check**（prompt 设计问题） |
| 05-S2 捕获类型错误 | ✅ PASS | **16/16** | 完美 |
| 05-S3 修复后清零 | ✅ PASS | 15/16 | 多次 check 错误减少 |
| **EXT-06 Hook** ||||
| 06-S1 PostToolUse | ✅ PASS | **17/17** | 完美 |
| 06-S2 Stop | ✅ PASS | **17/17** | 完美 |
| 06-S3 SessionStart | ✅ PASS | **17/17** | 完美 |

**总计：14/15 PASS（93%），5 个场景 17/17 满分**

---

## 2. 发现并修复的 5 个 bug

### Bug 1: cmd_run 没注册 memory/lsp_check 工具 ✅ 已修 (`54d31c2`)

之前 EXT-02/05 测试都是空跑——LLM 调不到工具。

### Bug 2: default_ci_checks 用 `| tail` 吞 cargo 退出码 ✅ 已修 (`1875661`)

cargo build 失败也判 PASS，GoalSupervisor false-finish 拦截失效。

### Bug 3: GlobalMemory 不是真"全局"（project-scoped）✅ 已修 (`e9c7cd7`)

X-2 跨 session workflow 揭示：`MemoryStore::search` 强制带 project 过滤。修复：加 `search_with_scope(global: bool)` + `memory_search` 工具加 `global` 参数。

### Bug 4: `tool_loop_detector.rs:151` UTF-8 panic ✅ 已修 (`e9c7cd7`)

03-S2 揭示：`trimmed[..50]` 字节切片在中文字符中间 panic。修复：`chars().take(50).collect()`。加回归测试。

### Bug 5: validate_html.py 多个产物路径错 ✅ 已修

- `02-M5` 检查 `global_memory.db`（underscore），实际是 `global-memory.db`（hyphen）→ commit `6b88f48`
- `04-M2/M3` 检查 `~/.ion/agent/snapshots/`，实际在 `~/.ion/file-store/<hash>/snapshots/` → commit `f75fc74`
- `03-M1` 只查 session.jsonl 格式，HTML base64 是 pi 格式 → commit `865cfd2`

---

## 3. 关键洞察

> **每个真实 bug 都是被"严谨测试"发现的——v1 全部漏检。**

| Bug | v1（之前） | v2（现在） |
|---|---|---|
| cmd_run 没注册工具 | 9/9 PASS（漏检） | 立刻暴露 |
| cargo exit code 被吞 | 9/9 PASS（漏检） | GoalSupervisor demo 立刻暴露 |
| GlobalMemory 不全局 | 9/9 PASS（漏检） | X-2 跨 session 立刻暴露 |
| UTF-8 panic | 9/9 PASS（漏检） | 03-S2 崩溃 exit 101 |
| 检查器路径错 | 假阴性 | 5 个场景的 warning 揭示 |

**这就是用户说"太简单、东西不够多"的真实代价**：浅层测试 = 漏 bug = 假通过率。

---

## 4. v2 基础设施产出

| 产出 | 文件 | 说明 |
|---|---|---|
| 测试矩阵设计 | `docs/design/EXT_TEST_MATRIX.md` | 5 exts × 6-8 场景 + 36 专属指标 + 5 工作流 |
| 扩展专属检查器 | `scripts/validate_html.py` | `--ext EXT-XX` 跑 6-8 专属指标 |
| 多场景批量跑 | `scripts/validate_ext_scenarios.sh` | 每场景独立 session + HTML + 校验 |
| 场景定义 | `scripts/ext_scenarios.sh` | 15 场景 + hook 配置 setup 函数 |
| 跨 session 工作流 | `scripts/workflow_cross_session.sh` | X-2 验证 memory 持久化 |
| HTML 可视化 | `src/export.rs` | 扩展视角统计 + 时间线条形图 |

---

## 5. 后续工作

| 优先级 | 工作 |
|---|---|
| P0 | 修 05-S1 prompt（让 LLM 主动调 lsp_check） |
| P0 | 实现 X-1（dev server + LSP 工作流） |
| P1 | HTML 加 I/O 对照表（args + result 并排折叠） |
| P1 | 跑性能场景（大文件 / 50+ memory entries / 长输出） |
| P2 | 实现 X-3/X-4/X-5（hooks 触发回滚等组合） |

---

## 6. 相关 commits（v2 完整链）

```
4380b95 fix(scenarios): 02-S2 expected metrics shouldn't include 02-M1
865cfd2 fix(validate): 03-M1 also check HTML pi format
f75fc74 fix(validate): 04-M2/M3 look in ~/.ion/file-store for snapshots
64ab046 test(memory): cover search_with_scope global=true (Bug A regression)
e9c7cd7 fix(memory,utf8): global search mode + tool_loop_detector UTF-8 panic
6b88f48 fix(validate): 02-M5 include global-memory.db (hyphen) in path
4f1f3e7 docs(ext): final validation report — 3 critical bugs found
202041e feat(export): add extension breakdown + timeline visualization
c6d0bd5 feat(workflow): X-2 cross-session memory persistence test
f67caf1 feat(validate): multi-scenario extension testing infrastructure
a5f5ca9 feat(validate): add extension-specific hard metrics (--ext EXT-XX)
7205c8f docs(ext): full test matrix
```

**总计 11 个 commit，~1500 行代码 + 文档**，覆盖：测试矩阵设计 / 专属检查器 / 多场景批量 / 跨 session 工作流 / HTML 可视化 / 5 bug 修复。
