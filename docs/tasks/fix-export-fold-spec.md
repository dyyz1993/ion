# Task Spec: 导出 HTML 的 tool result 默认折叠 + timeline 紧凑

> **状态：待 B 执行** | 改动范围：`src/export.rs`（只改导出模板的 JS/CSS）

## 背景

用户反馈导出的 session HTML 有两个问题：
1. **tool result 太长不折叠**——bash 输出、read 结果等完整展开，一屏看不到几条
2. **timeline 区域（id="ion-ext-viz"）内容太散**——间距大、不够紧凑

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

## 验证

```bash
# 1. 编译
cargo check 2>&1 | tail -3

# 2. 导出一个测试 HTML 看效果
target/debug/ion --session <any-sid> --export /tmp/test_fold.html

# 3. 打开看 tool result 是否折叠 + timeline 是否紧凑
open /tmp/test_fold.html

# 4. 测试通过
cargo test --lib export 2>&1 | tail -5
```

## 守门

- ✅ `cargo check` 无错误
- ✅ `cargo test --lib export` 全过
- ✅ 只改 src/export.rs
- ✅ 无 U+FFFD
