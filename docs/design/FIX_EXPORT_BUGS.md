# FIX: Session 导出 HTML 的 3 个 Bug（工具面板缺 skill + 消息内容不渲染）

> **状态：开发中** — spec 已写好，待 developer 实现。
> **类型：bug fix**（A→B 流程）
> **影响文件**：`src/export.rs`（主）+ `ion-provider/src/types.rs`（可选，Message 序列化）

## 背景

用 `--export` 导出 ion session 为 HTML 时，发现 3 个独立 bug，导致导出的 HTML：
1. 工具面板（Available Tools）**缺 skill 工具**
2. 工具面板**缺 bash_run 等扩展工具**（只有 18 个内核 builtin）
3. **消息内容完全不渲染**（页面只有 header 框架，对话正文空白）

## Bug 1：工具面板缺 skill

### 根因

`src/export.rs` 的 `export_session_rich()` 用 `registry.register_builtins()` 重建工具列表。但 `register_builtins()`（`src/agent/tool.rs:103`）**故意跳过了 SkillTool**：

```rust
// src/agent/tool.rs register_builtins() 末尾：
// SkillTool requires skill_dirs at construction time — skipped here.
```

SkillTool 构造需要 `skill_dirs` 参数，而 `register_builtins()` 是无参的，所以被跳过。

### 修复方案

在 `src/export.rs` 的 `export_session_rich()` 里，`register_builtins()` **之后**，手动注册 SkillTool（export.rs 在 ~行 153 已经构造了 `skill_tool` 用于 system prompt，可以复用它的 `skill_dirs`）：

```rust
// src/export.rs，在 registry.register_builtins() 之后（约行 167）加：
registry.register(Box::new(crate::agent::tool::SkillTool {
    skill_dirs: skill_dirs.clone(),  // 复用行 153 已构造的 skill_dirs
    disabled: crate::config::IonConfig::load().skills.disabled,
}));
```

注意：`skill_dirs` 变量在行 98-110 已经构造好（用于 system prompt 注入 skill 大纲），export.rs 里 register_builtins() 在行 167。需要在行 153 的 `skill_tool` 构造之后、行 167 之前，确保 `skill_dirs` 在作用域内（可能需要把 `let skill_dirs = ...;` 提前，或 clone）。

### 验证

```bash
# 导出任意 session，检查 tools 列表含 skill
SID=<任意 session id>
target/debug/ion --export /tmp/test.html --session "$SID"
# 解析 base64 数据，确认 tools 数组里有 {"name":"skill"}
python3 -c "
import base64,json,re
html=open('/tmp/test.html').read()
m=re.search(r'<script id=\"session-data\"[^>]*>(.*?)</script>',html,re.DOTALL)
data=json.loads(base64.b64decode(m.group(1)).decode())
print('含 skill:', any(t['name']=='skill' for t in data.get('tools',[])))
"
# 期望：含 skill: True
```

## Bug 2：消息内容不渲染（前端空白）

### 根因（格式不匹配）

**ion 存储 message 用 Rust enum externally-tagged 格式**（`ion-provider/src/types.rs:230`）：

```rust
#[derive(Serialize, Deserialize)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    ...
}
```

序列化成 JSON 是：
```json
{
  "message": {
    "User": {
      "role": "user",
      "content": [{"Text": {"text": "..."}}]
    }
  }
}
```

**但 pi 的前端模板 `template.js:44` 期望扁平格式**：
```js
if (entry.type === 'message' && entry.message.role === 'assistant') { ... }
// 期望：entry.message.role / entry.message.content（直接在 message 下）
```

`entry.message.role` 永远是 `undefined`（role 在 `message.User.role` 里），所以 **所有消息都被跳过，渲染空白**。

### 修复方案（推荐：export 端展平，不改 Message 序列化）

在 `src/export.rs` 把 entry 塞进 base64 **之前**，展平 message 格式。不改 `ion-provider` 的 Message enum（那个格式 session 存储/compaction 都依赖，改动风险大）。

在 `src/export.rs` 构建 `entries` 的地方（行 429 附近，`let mut entries: Vec<Value> = raw_entries...`），加一个展平步骤：

```rust
// src/export.rs，entries 构建完成后、base64 编码之前：
// Flatten Rust enum externally-tagged message format for pi template compatibility.
// {"message": {"User": {"role":"user","content":[...]}}} 
//   → {"message": {"role":"user","content":[...]}}
for entry in entries.iter_mut() {
    if entry.get("type").and_then(|v| v.as_str()) == Some("message") {
        if let Some(msg) = entry.get_mut("message").and_then(|m| m.as_object_mut()) {
            // 找 enum wrapper（User/Assistant/ToolResult/BashExecution/Custom/...）
            let wrapper_keys: Vec<String> = msg.keys()
                .filter(|k| matches!(k.as_str(), 
                    "User"|"Assistant"|"ToolResult"|"BashExecution"|"Custom"|"BranchSummary"|"CompactionSummary"))
                .cloned()
                .collect();
            for wrapper in wrapper_keys {
                if let Some(inner) = msg.remove(&wrapper) {
                    if let Some(inner_obj) = inner.as_object() {
                        // 把 inner 的字段（role/content/...）提升到 message 层
                        for (k, v) in inner_obj {
                            msg.insert(k, v);
                        }
                    }
                }
            }
        }
    }
}
```

**同样需要对 content blocks 展平**（如果 content 里也是 `{Text:{text}}` 格式）：

```rust
// content block 展平：{"Text":{"text":"..."}} → {"type":"text","text":"..."}
// 在 message 展平之后，遍历 content 数组
const BLOCK_MAP: &[(&str, &str)] = &[
    ("Text", "text"), ("ToolUse", "toolCall"), ("ToolCall", "toolCall"),
    ("ToolResult", "toolResult"), ("Thinking", "thinking"), ("Image", "image"),
];
if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
    for block in content.iter_mut() {
        if let Some(block_obj) = block.as_object_mut() {
            for (wrapper, type_name) in BLOCK_MAP {
                if let Some(inner) = block_obj.remove(*wrapper) {
                    block_obj.insert("type".to_string(), Value::String(type_name.to_string()));
                    if let Some(inner_obj) = inner.as_object() {
                        for (k, v) in inner_obj {
                            block_obj.insert(k, v);
                        }
                    }
                    break;
                }
            }
        }
    }
}
```

> **验证 content block 格式**：实现前先 dump 一个真实 session 的 content block 看实际 wrapper 名（可能是 `Text` 也可能已经是 `type:text`）。Bug 1 的验证脚本可以扩展来检查。

### 验证

```bash
# 导出 + 用 playwright 检查 DOM
SID=<任意 session id>
target/debug/ion --export /tmp/test.html --session "$SID"
node -e "
const {chromium}=require('playwright');
(async()=>{
  const b=await chromium.launch();
  const p=await b.newPage();
  await p.goto('file:///tmp/test.html',{waitUntil:'networkidle'});
  await p.waitForTimeout(1500);
  const r=await p.evaluate(()=>({
    msgs: document.querySelectorAll('#messages > *').length,
    len: document.body.innerText.length
  }));
  console.log('消息卡片数:', r.msgs, 'body文本长度:', r.len);
  // 期望：消息卡片数 > 0，body文本长度 > 1000
  await b.close();
})();
"
```

## Bug 3（设计改进，可选）：bash_run 不应是独立工具

### 现状

ion 有**两套 bash**：
- 内核 `bash`（`src/agent/tool.rs:771`）—— 只同步，参数只有 `command`
- 扩展 `bash_run`（`src/agent/bash.rs:100`）—— 支持 `background`/`timeoutBackground`

这导致导出 HTML 工具面板里出现 `bash_run`（割裂），用户体验上应该是**一个 bash 工具 + background 参数**。

### 改进方向（记录，不强制本次实现）

理想终态：统一成一个 `bash` 工具，`background: bool` 参数控制前后台。

实现路径（符合"内核原子 + 扩展封装"架构）：
1. **内核 `bash` 加 `background` 可选参数**（默认 false = 同步，true = 后台）
2. **后台逻辑仍在 BashExtension**（扩展封装策略，不塞进内核）
3. `bash_run` 保留为**内部实现**或废弃（LLM 只看到 `bash`）

这个改动较大（涉及 BashExtension 重构 + system prompt 调整），**建议单独开 spec**，本次只修 Bug 1 + Bug 2。

## 实现优先级

| Bug | 优先级 | 改动量 | 风险 |
|-----|--------|--------|------|
| Bug 2（消息不渲染）| **P0 必修** | 中（~30 行展平逻辑）| 低（只改 export 输出）|
| Bug 1（工具缺 skill）| **P0 必修** | 小（~5 行注册 SkillTool）| 低 |
| Bug 3（bash 统一）| P2 可选 | 大（架构重构）| 中 |

## 注意事项

1. **ALL COMMENTS MUST BE IN ENGLISH ONLY**（避免 U+FFFD）
2. Bug 2 的展平逻辑**只改 export 输出**，不动 session 存储格式（`ion-provider/src/types.rs` 的 Message enum 不变）
3. Bug 1 注册 SkillTool 时，确认 `skill_dirs` 变量在作用域内（export.rs 行 98-110 构造，行 167 使用，可能需要调整顺序或 clone）
4. **不改 pi 的 template.js**（那是外部依赖，ion 只读引用）
5. 实现前先 dump 一个真实 session 确认 content block 的实际格式（Text vs type:text）

## 改动清单（checklist）

- [ ] Bug 2: 在 `src/export.rs` entries 编码前加 message 展平逻辑（~30 行）
- [ ] Bug 2: 加 content block 展平逻辑（如果需要，~15 行）
- [ ] Bug 1: 在 `src/export.rs` register_builtins() 后注册 SkillTool（~5 行）
- [ ] `cargo build` 无 error/warning
- [ ] 命令行验证 Bug 1：导出 HTML 含 skill 工具
- [ ] 命令行验证 Bug 2：导出 HTML 消息卡片 > 0 + body 文本 > 1000
