# 扩展完整测试矩阵

> **状态**：定稿（2026-08-05）
> **目的**：替代「一个扩展跑一个 prompt」的浅层验证。每个扩展覆盖 6-8 个场景 + 5-8 个专属硬性指标 + 跨扩展组合工作流。

---

## 设计原则

1. **不只测 happy path**——必须覆盖边界、错误、并发、超长、持久化
2. **不只看通用 9 项**——每个扩展加专属硬性指标（函数级断言）
3. **不只单扩展测**——必须有跨扩展组合工作流（真实复杂场景）
4. **HTML 不只是文本**——加可视化（统计 / 时间线 / I/O 表 / 热图）

---

## EXT-02 GlobalMemoryExtension（8 场景 + 8 指标）

### 场景

| ID | 场景 | Prompt 要点 | 预期行为 |
|---|---|---|---|
| 02-S1 | save+search round-trip | "记住 X，然后搜 X" | save 返回 `gmem_<uuid>`，search 命中相同 ID |
| 02-S2 | 空查询 | "搜一个肯定没有的关键字 xyzqwerty" | search 返回 `[]` 或空数组，不报错 |
| 02-S3 | category 过滤 | save category=A → search 只在 A 里找 | 不匹配的 category 不返回 |
| 02-S4 | tags 多关键字 | save tags=[rust,linter] → search "linter" 命中 | 任一 tag 匹配即返回 |
| 02-S5 | 大量条目（≥50） | 循环 save 50 次后 search | 不超时（< 1s），返回正确子集 |
| 02-S6 | 跨 session 持久化 | session A save → session B search | B 能找到 A 写的（全局库） |
| 02-S7 | 超长 content（10KB） | save 一段 10KB 文本 | 不截断、不报错、search 能命中片段 |
| 02-S8 | 错误处理：缺 content | "调 memory_save 不传 content" | 工具返回明确错误，不 panic |

### 专属硬性指标

| ID | 指标 | 检查方法 |
|---|---|---|
| 02-M1 | memory_save 工具被调用 ≥ 1 次 | grep session.jsonl 的 ToolCall.name |
| 02-M2 | 返回的 ID 格式 `gmem_<uuid>` | 正则 `^gmem_[a-f0-9-]{36}$` |
| 02-M3 | memory_search 工具被调用 ≥ 1 次 | 同 M1 |
| 02-M4 | save 后 search 命中相同 ID | session.jsonl 里 save 的 ID == search 结果 ID |
| 02-M5 | 全局库文件存在 | `~/.ion/agent/global_memory.db` 或对应 jsonl 存在 |
| 02-M6 | 持久化：跨 session 能找到 | session B search 结果含 session A 的 ID |
| 02-M7 | HTML 里 memory 调用可见 | dump-dom 有 `memory_save` / `memory_search` 文本 |
| 02-M8 | 无 "memory tool not found" 错误 | session.jsonl 无 ToolResult.is_error=true on memory_* |

---

## EXT-03 DevServerDetectorExtension（7 场景 + 6 指标）

### 场景

| ID | 场景 | Prompt 要点 | 预期 |
|---|---|---|---|
| 03-S1 | 单 Python http.server | `python3 -m http.server 8765 &` | 检测到 8765，PID 记录 |
| 03-S2 | 不同端口（3000） | `python3 -m http.server 3000 &` | 检测到 3000 |
| 03-S3 | 多个 dev server 并发 | 启 8765 + 8766 + 8767 | 三个都被检测 |
| 03-S4 | Vite/Node 生态 | `npx vite --port 5173 &` | 检测到 5173 |
| 03-S5 | 进程退出后清除 | 启动后 kill PID | 下次扫描不再列 |
| 03-S6 | system prompt 注入 | 触发后让 agent 报告 dev_servers | assistant 提到端口号 |
| 03-S7 | 端口被占用（启动失败） | 占用 8765 后再启 | 不误报，记录失败原因 |

### 专属硬性指标

| ID | 指标 | 检查方法 |
|---|---|---|
| 03-M1 | bash 后台启动命令被调用 ≥ 1 次 | session.jsonl ToolCall=bash background=true |
| 03-M2 | 检测到的端口号出现在 HTML | grep `[8765|3000|5173]` in dom |
| 03-M3 | DevServer 注入到 system prompt（或消息） | session.jsonl 含 `dev_servers` 或 `<dev_servers>` |
| 03-M4 | 进程 PID 被记录 | HTML/JSONL 含 PID 字段 |
| 03-M5 | assistant 提到 dev server | assistant text 含 "detected" 或端口号 |
| 03-M6 | 无 "no dev server" 误报 | 启动了 server 但报告"未检测到"= FAIL |

---

## EXT-04 FileSnapshotExtension（8 场景 + 7 指标）

### 场景

| ID | 场景 | Prompt 要点 | 预期 |
|---|---|---|---|
| 04-S1 | 单文件创建 | write /tmp/x.txt "hello" | snapshot 记录 +1 |
| 04-S2 | 多文件创建 | write 3 个文件 | snapshot 记录 +3 |
| 04-S3 | 文件编辑（覆盖） | write 同路径不同内容 | snapshot 记录 diff |
| 04-S4 | 大文件（1MB） | write 大文件 | 不超时，snapshot 不卡 |
| 04-S5 | 跨目录 | write 到不同子目录 | 全部记录 |
| 04-S6 | 编辑 + 回滚 | write → rollback 到上一版本 | 文件内容回退 |
| 04-S7 | 删除文件 | rm 后检查 snapshot | 记录删除事件 |
| 04-S8 | 二进制文件（PNG） | write 二进制 | 不破坏内容 |

### 专属硬性指标

| ID | 指标 | 检查方法 |
|---|---|---|
| 04-M1 | write 工具被调用 ≥ 1 次 | session.jsonl ToolCall=write |
| 04-M2 | snapshot store 文件存在 | `~/.ion/agent/snapshots/` 不空 |
| 04-M3 | snapshot 数量 == write 次数 | 计数对齐 |
| 04-M4 | HTML 里 "snapshot" 关键字 ≥ 1 次 | dump-dom grep |
| 04-M5 | diff 在 HTML 里可折叠展示 | dump-dom 有 `output-collapsed` 或类似 class |
| 04-M6 | rollback（若调用）回退正确 | 回滚后文件 hash 匹配前一版 |
| 04-M7 | 无 "snapshot write failed" 错误 | session.jsonl 无相关 is_error |

---

## EXT-05 LspExtension（6 场景 + 7 指标）

### 场景

| ID | 场景 | Prompt 要点 | 预期 |
|---|---|---|---|
| 05-S1 | 干净代码 lsp_check | "写一个正确的 add 函数然后 lsp_check" | 0 errors, 0 warnings |
| 05-S2 | 引入编译错误 | "写一个 let x: i32 = \"string\"; 然后 lsp_check" | 报错，类型不匹配 |
| 05-S3 | 修复后再 check | S2 后修类型，再 lsp_check | errors 清零 |
| 05-S4 | 警告（unused） | "写 unused variable 然后 lsp_check" | 有 warning，0 errors |
| 05-S5 | 多文件交叉引用 | mod a 引用 b，b 有错 | lsp_check 报 b 的错 |
| 05-S6 | 超大文件（≥2000 行） | lsp_check 大文件 | 不超时 |

### 专属硬性指标

| ID | 指标 | 检查方法 |
|---|---|---|
| 05-M1 | lsp_check 工具被调用 ≥ 1 次 | session.jsonl ToolCall=lsp_check |
| 05-M2 | cargo check 真实执行 | HTML/bash_result 含 "cargo check" 输出 |
| 05-M3 | 错误能被捕获（S2） | session.jsonl ToolResult 含 "error\[E" |
| 05-M4 | 修复后错误清零（S3） | 第二次 lsp_check 结果无 "error\[E" |
| 05-M5 | 警告分类正确（warning vs error） | 有 warning 但 0 error（S4） |
| 05-M6 | HTML 里 lsp_check 输出可见 | dump-dom 有 `diagnostic` 或 `cargo check` |
| 05-M7 | 无 "lsp not initialized" 错误 | session.jsonl 无相关 is_error |

---

## EXT-06 HookExtension（8 场景 + 8 指标）

### 场景（覆盖 13 个事件中的关键 8 个）

| ID | 场景 | 配置（hooks.json） | 预期 |
|---|---|---|---|
| 06-S1 | PostToolUse command | `PostToolUse: command echo` | 命令执行，日志有记录 |
| 06-S2 | Stop command | `Stop: command echo stop >> log` | session 结束时触发 |
| 06-S3 | SessionStart | `SessionStart: command echo start` | session 启动时触发 |
| 06-S4 | PreToolUse matcher | `PreToolUse matcher=bash` | 仅 bash 工具触发，read/write 不触发 |
| 06-S5 | if 条件 | `if: "Bash(rm *)"` | 仅 rm 命令触发 |
| 06-S6 | async + rewake | `async: true, async_rewake: true` exit 2 | 注入 nextTurn 消息 |
| 06-S7 | disableAllHooks | `disableAllHooks: true` | 不触发任何 hook |
| 06-S8 | prompt 类型 | `type: prompt` | LLM 收到 prompt 注入 |

### 专属硬性指标

| ID | 指标 | 检查方法 |
|---|---|---|
| 06-M1 | hook 命令实际执行 | 日志文件存在且非空 |
| 06-M2 | 触发次数符合预期 | S4 只触发 bash，S7 完全不触发 |
| 06-M3 | matcher 过滤生效 | S4 中 read/write 不触发 |
| 06-M4 | if 条件生效 | S5 中非 rm 不触发 |
| 06-M5 | disableAllHooks 紧急逃生 | S7 中无任何触发 |
| 06-M6 | async_rewake 注入消息 | S6 中 nextTurn 消息存在 |
| 06-M7 | prompt 类型注入 LLM | S8 中 LLM 看到 prompt |
| 06-M8 | HTML 里 hook 触发记录可见 | dump-dom 有 hook 事件名 |

---

## 跨扩展组合工作流（5 个）

| ID | 工作流 | 涉及扩展 | 关键步骤 |
|---|---|---|---|
| X-1 | dev server + 文件改动 + LSP 验证 | bash + DevServer + FileSnapshot + LSP | 启 server → 改代码 → lsp_check → 看 server 是否热重载 |
| X-2 | 学经验 → 新 session 应用 | memory + 多 session | session A save 经验 → session B 启动 → search 应用 |
| X-3 | hook 触发 → 文件回滚 | hooks + FileSnapshot | Stop hook 检测到错误 → rollback 上次 write |
| X-4 | bash 失败 → memory 记录教训 | bash + memory | bash exit≠0 → memory_save 失败原因 → 下次类似命令前 memory_search |
| X-5 | 完整开发循环 | 全部 5 个 | 启 server + 改代码 + lsp_check + memory_save + hook 日志 |

---

## 实施估算

| 阶段 | 工作量 | 产出 |
|---|---|---|
| 阶段 2：扩 validate_html.py | 中（加 5 个 check 函数） | ~150 行 Python |
| 阶段 3：改 validate_extension.sh | 中（多场景循环） | ~80 行 bash |
| 阶段 4：组合工作流脚本 | 大（5 个工作流各 1 脚本） | ~250 行 bash |
| 阶段 5：HTML 可视化 | 大（4 类可视化） | ~300 行 Rust + JS |
| 阶段 6：跑测试 + 报告 | 中（37 场景 + 5 工作流） | 最终 HTML 报告 |

总计：**37 个扩展场景 + 5 个组合工作流 = 42 个测试运行**，覆盖 36 个专属硬性指标。
