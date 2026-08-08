# Task Spec: 导出 HTML 的长 Entry 渐进折叠 + Timeline 紧凑

> **状态：开发中** — 渐进折叠、响应式布局与完整 Entry Timeline 已通过 CLI 验证，视觉验收待手动刷新本地文件完成。

## 背景

用户反馈导出的 session HTML 有三个问题：
1. **tool result 太长不折叠**——bash 输出、read 结果等完整展开，一屏看不到几条
2. **timeline 区域（id="ion-ext-viz"）内容太散**——间距大、不够紧凑
3. **只有 tool result 会折叠**——User、Assistant、Custom 等 Entry 仍完整展开，正文密度不一致

## 改动 1: tool result 默认折叠

在 `src/export.rs` 的 pi 模板 JS 里，找到 toolResult 的渲染逻辑，用 `<details>` 标签包裹完整输出。

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

`turn_summary` 是回合边界、状态、工具数量、Token、耗时和还原定位使用的内部元数据，
不是独立的会话事件。导出时原样保存在 `internalEntries` 中，确保单文件 HTML 不丢数据，
但不进入正文、Timeline 或类型筛选，也不得转换成 `custom_message`。

分支会话先通过最后一条 `leaf_pointer` 解析 active leaf。正文与 Timeline 只使用
`root → active leaf` 的当前消息路径，同时保留全局审计 Entry，以及
`leaf_pointer` / `label` / `branch_summary` 这些分叉记录。废弃兄弟分支中的消息和
局部 Custom 数据不进入正文；线性会话则完整保留 `session.jsonl` 中的所有 Entry。

分类规则：

- 用户可见的内核原生 entry 保留真实类型，例如 `compaction`、`branch_summary`、
  `model_change`、`deletion` 和 `restoration`。
- 内部元数据 `turn_summary` 只进入 `internalEntries`，不参与可见类型分类。
- `message` 按 `user`、`assistant`、`tool result` 三种角色显示。
- Extension 产生的 `custom` / `custom_message` 统一显示为 `custom`，不按
  `customType` 拆分颜色或筛选项。
- 未知类型自动生成稳定颜色并加入筛选区，不能从 Timeline 消失。

交互要求：

- 每条 entry 对应一根按原始顺序紧凑排列的时间线细竖线，不限制最多 50 条；
  timestamp 只用于首尾范围和悬停详情，不能把会话中的空闲时间渲染成大片空白。
- 筛选后保留原始 Entry 槽位，不重新压缩顺序，便于看出被隐藏类型穿插的位置。
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
- 默认展示前 6 个视觉行；短 Entry 完整显示，不出现多余的展开按钮。
- 长 Entry 在预览下方显示近似剩余视觉行数及 `click to expand` 提示。
- User、Assistant、Tool Result、Custom、Compaction、Branch Summary、Model Change
  使用各自的类型标签和颜色，但共享同一套交互。
- 点击提示展开原始内容，再次点击 `Collapse` 收起；按钮同步维护 `aria-expanded`。
- 原始 Markdown、图片、复制链接、Thinking 与工具输出节点保留在折叠内容区，不重新序列化。
- pi 的分支导航会重建 `#messages`，使用 `MutationObserver` 为新节点重新应用折叠包装。
- Timeline 跳转到嵌套 Tool Result 时，先展开所属 Entry，再滚动和高亮目标。

### Design QA（2026-08-08）

- source visual truth: `codex-clipboard-f6ba1a3f-0088-4c12-be43-15d3f56e7589.png`
- implementation: `output/playwright/export-timeline-large.html`
- state: 长 Entry 默认预览、尚未展开
- browser evidence: 本地 `file://` 页面自动重载被 Browser 安全策略阻止，无法取得同视口实现截图
- interaction evidence: CLI 结构检查与 JavaScript 语法检查通过；视觉点击待用户手动刷新验证
- final result: blocked

## 验证

```bash
# 1. 编译
cargo check 2>&1 | tail -3

# 2. 导出一个测试 HTML 看效果
target/debug/ion --session <any-sid> --export /tmp/test_fold.html

# 3. 打开看长 Entry 是否保留 6 行内容预览 + timeline 是否紧凑
open /tmp/test_fold.html

# 4. 测试通过
cargo test --lib export 2>&1 | tail -5
```

## 守门

- ✅ `cargo check` 无错误
- ✅ `tests/export_ci.sh` 37/37
- ✅ 导出脚本可由 `node --check` 解析
- ✅ 无 U+FFFD
