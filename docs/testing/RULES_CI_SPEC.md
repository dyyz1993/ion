# Rules Engine CLI/CI 测试规格

> **状态：已完成** — 基于 KB 知识库 pi rules-engine 对比提炼，覆盖 ION rules engine 全链路。

## 一、测试覆盖矩阵

### Group A: 基础加载（文件扫描 + frontmatter 解析）

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| A1 | `.ion/rules/*.md` 文件扫描 | 创建 2 个 rule md，export HTML | systemPrompt 含 `<project_rules>` |
| A2 | 无 frontmatter 的 rule（全局） | 创建无 frontmatter 的 md | 分类为全局 rule，进 system prompt |
| A3 | `globs: "**"` 全局 rule | 创建 globs:** 的 md | 进 system prompt |
| A4 | `globs: "**/*.rs"` 路径匹配 rule | 项目有 .rs 文件 | 进 tool result，不进 system prompt |
| A5 | 不匹配的 rule（`**/*.py`，无 .py） | 项目无 .py 文件 | 不注入（既不进 SP 也不进 TR） |

### Group B: Glob 匹配

| ID | 验证点 | 测试数据 | 预期 |
|----|--------|---------|------|
| B1 | `**/*.rs` 匹配 src/lib.rs | `matches_file("src/lib.rs")` | true |
| B2 | `**/*.rs` 不匹配 src/lib.py | `matches_file("src/lib.py")` | false |
| B3 | `src/**` 匹配 src/a/b.rs | `matches_file("src/a/b.rs")` | true |
| B4 | `*.{ts,tsx}` 花括号展开（pi 有） | `matches_file("App.tsx")` | true（暂不开发，低优先级）|
| B5 | `!**/*.d.ts` 排除模式（pi 有） | `matches_file("types.d.ts")` | false（暂不开发，低优先级）|

### Group C: 注入位置（双路径）

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| C1 | 全局 rule 在 system prompt | export HTML，解析 systemPrompt | 含 `<project_rules>` + 全局 rule 内容 |
| C2 | 路径匹配 rule 在 tool result | export HTML，解析 entries 的 ToolResult | 含 `📌 [project rules for this file]` |
| C3 | export 不把路径匹配 rule 塞 system prompt | export HTML systemPrompt | 不含路径匹配 rule（只含全局）|
| C4 | bash 工具不追加 rules | LLM 调 bash，检查 tool result | 不含 `📌 [project rules]` |

### Group D: 去重 + TTL 清理

| ID | 验证点 | 方法 | 预期 |
|----|--------|------|------|
| D1 | 同一 rule 只追加一次（不管多少文件）| 单元测试：mock 两个 .rs 文件调用 after_tool_call | 第二次不追加 |
| D2 | 20 轮后 TTL 清空（rule 可重注入）| 单元测试：模拟 20 轮 on_system_prompt | injected set 清空 |
| D3 | 不同 rule 各自独立去重 | rust rule + testing rule 同时匹配 | 两个都追加（各自独立）|

### Group E: extension_rpc（CLI 直调）

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| E1 | list 返回所有 rules | `ion rpc --method extension_rpc --params '{"method":"list"}'` | rules 数组 ≥1 |
| E2 | match 按文件匹配 | `--params '{"method":"match","args":{"file":"src/lib.rs"}}'` | 含匹配的 rule |

## 二、跟 pi 对比的差异（KB 提炼）

### 已对齐 ✅

| 特性 | pi | ION | 状态 |
|------|----|----|------|
| 无条件 rule → system prompt | before_agent_start 注入 `<system-reminder>` | on_system_prompt 注入 `<project_rules>` | ✅ |
| 条件 rule → tool result | tool_result 事件追加 | after_tool_call 追加 | ✅ |
| 去重 | loaded/already_loaded/reloaded 三态 | injected set（rule.name 去重） | ✅ 基本对齐 |
| TTL 清理 | 30s 文件缓存 TTL | 20 轮 turn TTL（injected set 清理） | ✅ 不同粒度但等效 |
| glob 匹配引擎 | ignore 库（npm） | 自研 glob_match | ✅ |
| 热重载 | 每次读盘（cache 30s） | 每次读盘（无缓存） | ✅ |
| 压缩后清理 | session_compact 清空所有状态 | TTL 清理（间接） | ⚠️ 部分（ION 无 session_compact 钩子）|

### 待对齐 ❌

| 特性 | pi | ION | 优先级 |
|------|----|----|--------|
| frontmatter 字段名 | `globs`（主）+ `paths`（别名兼容） | `globs` | P1（应同时支持 globs/paths/globs）|
| 花括号展开 `*.{ts,tsx}` | ✅（ignore 库支持） | ❌ | P2 |
| 排除模式 `!**/*.d.ts` | ✅（ignore 库支持） | ❌ | P2 |
| severity/description 扩展字段 | ✅ | ❌ | P3（不影响功能）|
| 三态去重（reloaded） | ✅（entry 失效后重新注入）| ❌（只有 loaded/already_loaded 两态）| P2 |
| session_compact 清理 | ✅（压缩后清空所有追踪）| ❌ | P2 |
| `.mdc` 文件格式 | ✅（Cursor 兼容） | ❌（只 .md） | P3 |
| 多目录扫描 | ✅（4 级：全局/用户/企业/项目） | ✅（2 级：~/.ion + 项目）| P3（按需加）|

## 三、CI 脚本执行

```bash
bash tests/rules_ci.sh
```

覆盖 Group A（export 验证）+ Group C（注入位置）+ Group D（去重单元测试）。

## 四、暂不开发的 pi 对齐项（低优先级）

ION 当前 28 个单元测试，pi 有 115+。主要差距：

| pi 测试组 | 用例数 | ION 对应 | 需补 |
|----------|--------|---------|------|
| matcher.test.ts | 27 | glob_match 单元测试 | 花括号 + 排除模式 |
| loader.test.ts | 37 | parse_frontmatter + load_rules | globs/paths 字段名兼容 |
| injector.test.ts | 12 | format_rules_xml | 基本覆盖 |
| cache.test.ts | 7 | 无（ION 无文件缓存） | N/A |
| conditional-dedup.test.ts | 3 | injected set 去重 | reloaded 三态 |
| lifecycle.test.ts | 10 | 无（集成测试） | 需补集成测试 |
