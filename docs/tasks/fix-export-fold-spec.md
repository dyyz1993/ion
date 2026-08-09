# Task Spec: 导出 HTML 的长 Entry 渐进折叠 + Timeline 紧凑

> **状态：已验证** — ION 自有单文件模板、完整 Entry Timeline、流程语义、Custom 来源、受众标记与渐进折叠均已完成，并通过 CLI 与浏览器交互验证。

## 背景

用户反馈导出的 session HTML 有三个问题：
1. **tool result 太长不折叠**——bash 输出、read 结果等完整展开，一屏看不到几条
2. **timeline 区域（id="ion-ext-viz"）内容太散**——间距大、不够紧凑
3. **只有 tool result 会折叠**——User、Assistant、Custom 等 Entry 仍完整展开，正文密度不一致

## 改动 0：ION 自有模板与流程还原

导出 HTML 是 ION Session JSONL 的离线审计视图，不依赖开发机上的 pi 源码目录。pi 仅作为交互和数据呈现参考；模板、样式、脚本及第三方离线资源必须纳入 ION 并编译进二进制。

每条可见 Entry 在导出数据中增加只读的 `ionMeta`（不改写 Session JSONL），至少包含：

- `phase`：用户输入、上下文注入、LLM 响应、工具请求、工具结果、Extension 事件或 Session 控制。
- `source`：用户、LLM、工具、内核或具体 Extension。
- `sourceConfidence`：`recorded` / `inferred` / `unknown`，历史数据来源不完整时禁止猜测成确定事实。
- `audience`：是否进入 LLM 上下文、是否在实时 UI 展示、是否作为审计数据保留。
- `customType` 与 `displayType`：内置 Custom 使用具体名称；无法识别的运行时 Extension Custom 统一显示为 `Custom`。

语义约束：

- `message.User`：用户或注入输入，进入 LLM 上下文。
- `message.Assistant`：一次 LLM 响应，展示 provider/model/API、Token、stop reason 与工具请求。
- `message.ToolResult`：工具响应；若被 Hook 拒绝，拒绝信息仍位于 ToolResult，Hook Entry 只是旁路审计。
- `message.Custom`：会话上下文中的 Custom，可被后续 LLM 调用看到；`display` 仅控制实时 UI。
- 顶层 `custom_message`：当前 ION 恢复逻辑不把它装载进模型消息，按 UI/审计记录处理。
- 顶层 `custom`、`system_event`：旁路或内部元数据，不宣称被 LLM 消费。

页面顶部提供流程摘要：LLM 调用数、工具请求/结果数、Custom 数、涉及的 Extension；Timeline 悬浮与正文卡片必须使用同一份 `ionMeta`。类型目录必须同时展示 ION 固定的 17 种 Session Entry、导出器识别的 25 种内置 Custom，以及当前会话实际出现的 raw Entry、可见类型、Message role、Custom 和 Extension 数；运行时 Extension Custom 明确标记为开放集合。

## 改动 1: tool result 默认折叠

在 ION 内置的导出模板 JS 中，找到 toolResult 的渲染逻辑，用 `<details>` 标签包裹完整输出。

### 当前行为
tool result 的输出直接渲染，超长时用 `formatExpandableOutput(output, 10)` 做头尾折叠（但仍占大量空间）。

### 期望行为
tool result 默认折叠——只显示工具名 + 一行摘要（前 80 字符），点击 `<summary>` 展开完整输出。

### 改法

在 `src/export.rs` 里找到渲染 toolResult 的 JS 模板代码（大约在 `default_args_new` 附近的 `formatExpandableOutput` 调用处）。

把 tool output 的渲染从：
```javascript
html += `<div class="tool-output"><pre>${escapeHtml(output)}</pre></div>`;
```

改成：
```javascript
const preview = output.substring(0, 100).replace(/\n/g, ' ');
html += `<details class="tool-result-fold"><summary class="tool-result-preview">${escapeHtml(preview)}${output.length > 100 ? '...' : ''}</summary><div class="tool-output"><pre>${escapeHtml(output)}</pre></div></details>`;
```

同时在 CSS 里加：
```css
.tool-result-fold { margin: 4px 0; }
.tool-result-fold summary { cursor: pointer; color: #8b949e; font-size: 12px; padding: 2px 8px; background: #f6f8fa; border-radius: 4px; }
.tool-result-fold summary:hover { background: #e1e4e8; }
.tool-result-fold[open] summary { margin-bottom: 4px; }
```

### 注意

- `formatExpandableOutput` 的逻辑保留（用于 details 展开后的超长输出折叠）
- bash_result（custom message）也要同样折叠
- 工具名 + 参数仍显示在 tool-header 里（不折叠）

## 改动 2: timeline（ion-ext-viz）紧凑化

在 `src/export.rs` 里找到 `ion-ext-viz` 的 CSS/HTML，把间距改小。

### 当前行为
timeline 里每个事件之间间距太大，一屏显示不了几个。

### 期望行为
紧凑——减小 padding/margin/gap，事件行高紧凑。

### 改法

找到 `ion-ext-viz` 相关的 CSS（在 export.rs 的 CSS 注入段），减小：
- `padding: 16px` → `padding: 8px`
- `gap: 20px` → `gap: 6px`
- `margin-bottom` → 减半
- 字体大小可以微调小一号

## 改动 3: 完整 Entry Timeline + 交互筛选

Timeline 的 `timelineEntries` 与正文 `entries` 使用同一条完整的用户可见事件流，
并按模板需要做格式转换。`compaction`、状态切换等真实生命周期 entry 既出现在
Timeline，也必须在正文中拥有可见展示和稳定锚点。

回合边界使用真实 user message entry；状态、工具数量和 Token 从 Assistant/ToolResult
消息派生，会话总量来自 SessionIndex。文件还原定位使用消息树上的 parented
`customType: "step-snapshot"`，它是独立生命周期事实，必须进入正文、Timeline 和类型筛选。

分支会话先通过最后一条 `leaf_pointer` 解析 active leaf。正文与 Timeline 只使用
`root → active leaf` 的当前消息路径，同时保留全局审计 Entry，以及
`leaf_pointer` / `label` / `branch_summary` 这些分叉记录。废弃兄弟分支中的消息和
局部 Custom 数据不进入正文；线性会话则完整保留 `session.jsonl` 中的所有 Entry。

分类规则：

- 用户可见的内核原生 entry 保留真实类型，例如 `compaction`、`branch_summary`、
  `model_change`、`deletion` 和 `restoration`。
- `custom:step-snapshot` 显示为 File Snapshot，并在悬停概要中展示文件数和 tree hash。
- `message` 按 `user`、`assistant`、`tool result` 三种角色显示。
- 已知的内置 Custom（如 Hook、Diagnostics、Memory、Compaction）使用具体名称、
  稳定颜色与独立筛选项；无法识别的运行时 Extension Custom 统一显示为 `Custom`，
  同时保留原始 `customType` 与准确的 Extension 来源。
- 未知类型自动生成稳定颜色并加入筛选区，不能从 Timeline 消失。

交互要求：

- 每条 entry 对应一根按原始顺序紧凑排列的时间线细竖线，不限制最多 50 条；
  timestamp 只用于首尾范围和悬停详情，不能把会话中的空闲时间渲染成大片空白。
- 筛选同时作用于 Timeline 与正文；剩余 Entry 保持原始相对顺序并紧凑排列。
- 鼠标悬停、键盘聚焦时，显示 entry 类型、序号、时间、ID 和
  截断后的内容概要。
- 点击任意竖线时，平滑滚动到对应正文内容并短暂高亮；不允许存在只有 Timeline
  标记、正文没有目标的 Entry。
- ToolResult 使用 Assistant 卡片内对应 Tool Call 作为正文目标，不重复生成顶层卡片。
- `compaction` 使用独立的内置压缩卡片；模板不认识的其他内置类型使用紧凑通用卡片。
- `hook_event` 保留自身锚点，但优先嵌入带 `toolCallId` 的 Tool Result；旧数据没有
  关联 ID 时嵌入下一条 Turn Summary，避免形成连续的顶层 Hook 卡片。
- 类型按钮显示数量；点击后隐藏/恢复该类型，并实时更新可见 entry 数。
- `Show all` 一键恢复全部类型。
- 原有 Agent、Date、Models、ION Version、Messages、Tool Calls、Tokens、Cost
  元信息卡必须保留；命中的 `SessionIndex.SessionMeta` 快照写入
  `header.indexMeta`，离线页面可直接读取索引元信息。

## 改动 4: 所有正文 Entry 统一渐进折叠

- `#messages` 下所有带 `entry-*` ID 的可见 Entry 保留真实渲染内容。
- 角色标题、时间和复制按钮不计入折叠阈值；折叠预览默认展示 3 个正文视觉行。
- 隐藏部分不超过 3 个视觉行时，Entry 直接完整显示；只有展开后能新增超过 3 行正文时才折叠。
- Tool Result 内层的“查看剩余内容”与 `formatExpandableOutput` 使用同一门槛，禁止详情只有 1～3 行时仍显示展开操作。
- 长 Entry 在预览下方显示近似剩余视觉行数及 `click to expand` 提示。
- User、Assistant、Tool Result、Custom、Compaction、Branch Summary、Model Change
  使用各自的类型标签和颜色，但共享同一套交互。
- 点击提示展开原始内容，再次点击 `Collapse` 收起；按钮同步维护 `aria-expanded`。
- 原始 Markdown、图片、复制链接、Thinking 与工具输出节点保留在折叠内容区，不重新序列化。
- 模板的分支导航会重建 `#messages`，使用 `MutationObserver` 为新节点重新应用折叠包装。
- Timeline 跳转到嵌套 Tool Result 时，先展开所属 Entry，再滚动和高亮目标。

### Design QA（2026-08-09）

- representative fixture: `tests/fixtures/export/flow_semantics/session.jsonl`
- implementation: `output/playwright/export-flow-complete-20260809.html`
- state: 单文件离线导出；Timeline 与正文包含 active branch 的真实消息及 File Snapshot
- browser evidence: Playwright 验证 Flow Summary、17/25 类型目录、筛选、Custom 悬停来源概要、点击跳转与正文 provenance；导出本身无需服务器
- interaction evidence: Custom 筛选后 Timeline 从 7 条变 6 条，正文对应 weather Custom 同步隐藏；悬停显示 `source weather`、`audit only`、`hidden in live UI`
- final result: `tests/export_ci.sh` 54/54 passed；真实 ION/Faux CLI 均验证 parented File Snapshot

## 验证

```bash
# 1. 编译
cargo check 2>&1 | tail -3

# 2. 导出一个测试 HTML 看效果
target/debug/ion --session <any-sid> --export /tmp/test_fold.html

# 3. 打开看长 Entry 是否至少保留 3 行正文预览 + timeline 是否紧凑
open /tmp/test_fold.html

# 4. 测试通过
cargo test --lib export 2>&1 | tail -5
```

## 守门

- ✅ `cargo check` 无错误
- ✅ `tests/export_ci.sh` 54/54（含真实 ION host + FauxProvider + CLI 导出与类型目录）
- ✅ `cargo test --lib export::tests` 23/23
- ✅ `tests/hooks_pretool_deny_ci.sh` 8/8（真实 ION Hook 拒绝链路 + 类型目录来源）
- ✅ Extension `on_context` 来源标记与 Memory Custom 注入测试通过
- ✅ Playwright：悬停、筛选、跳转、正文来源标签通过
- ✅ 真实 LLM case 已登记：`ION_E2E_CLI=1 cargo test --test cli_e2e_real e2e_html_export_flow -- --ignored --nocapture`
- ✅ 导出脚本可由 `node --check` 解析
- ✅ 无 U+FFFD
