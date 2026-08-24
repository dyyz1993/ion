> ⚠️ **本文档中的 bash_run / bash_manage 已过时。** 当前工具名：`bash`（执行）+ `get_background_process`（查状态）+ `kill_process`（杀进程）+ `write_stdin`（发 stdin）。详见 BASH_EXTENSION.md。

# FEAT: Extension trait 加 register_tools 钩子（工具自描述）

> **状态：开发中** — spec 已写好，待实现。
> **类型：架构改进**（治本，解决 export 漏扩展工具 + 为 bash 统一铺路）
> **影响文件**：`src/agent/extension.rs` / `src/agent/bash.rs` / `src/bin/ion.rs` / `src/worker_rpc.rs` / `src/export.rs`

## 背景

export 导出 session HTML 时，工具面板（Available Tools）丢失扩展工具（skill/bash_run/lsp/goal/plan/mcp/wasm）。

根因：**工具注册没有统一扩展点**。
- 内核工具走 `ToolRegistry::register_builtins()`（`src/agent/tool.rs:103-128`），只注册无状态的 15 个
- 扩展工具靠 `worker_rpc.rs:1109-1132` 等**散落的手动 `agent.register_tool(...)`** 注册
- export（`src/export.rs:166`）重建工具列表时只调 `register_builtins()`，拿不到扩展工具

注释承认了这个缺口（`tool.rs:126-127`）：
```rust
// SkillTool requires skill_dirs at construction time — skipped here.
// It's registered separately in ion_worker.rs / ion.rs with proper config.
```

## 方案：Extension trait 加 `register_tools` 钩子

让每个扩展**自描述**自己的工具。worker 启动 + export 重建时，遍历所有扩展调 `register_tools`，自动拿到完整工具集。

### 改动 1：Extension trait 加默认空方法（`src/agent/extension.rs`）

在 Extension trait 定义里（约行 96-368），加一个默认 no-op 方法：

```rust
/// Register this extension's tools into the registry.
/// Default: no-op. Override to expose extension-specific tools
/// (e.g. BashExtension registers bash_run/bash_kill/bash_send/bash_bg).
/// Called by worker startup AND export tool-reconstruction.
fn register_tools(&self, _registry: &mut crate::agent::tool::ToolRegistry) {}
```

放在其他钩子之间（比如 `on_system_prompt` 附近），保持分组。

### 改动 2：BashExtension impl register_tools（`src/agent/bash.rs`）

BashExtension 持有 process_map/stdin_map/notify_map/follow_up_tx/storage（行 721-727）。当前 4 个工具在 `worker_rpc.rs:1109-1132` 手动注册。

在 `impl Extension for BashExtension`（行 758+）里加：

```rust
fn register_tools(&self, registry: &mut crate::agent::tool::ToolRegistry) {
    registry.register(Box::new(BashRunTool {
        process_map: self.process_map.clone(),
        stdin_map: self.stdin_map.clone(),
        notify_map: self.notify_map.clone(),
        follow_up_tx: self.follow_up_tx.clone(),
        storage: self.storage.clone(),
    }));
    registry.register(Box::new(BashKillTool {
        process_map: self.process_map.clone(),
    }));
    registry.register(Box::new(BashSendTool {
        stdin_map: self.stdin_map.clone(),
        process_map: self.process_map.clone(),
    }));
    registry.register(Box::new(BashBackgroundTool {
        process_map: self.process_map.clone(),
        stdin_map: self.stdin_map.clone(),
    }));
}
```

**注意**：实现前先读 `src/agent/bash.rs` 的 BashRunTool/BashKillTool/BashSendTool/BashBackgroundTool 的 struct 定义（约行 100/523/603/680），确认它们的字段（需要哪些 Arc clone）。worker_rpc.rs:1109-1132 是现成的参考——直接搬字段。

### 改动 3：worker 启动时遍历扩展注册工具（`src/worker_rpc.rs` + `src/bin/ion.rs`）

**当前**（`worker_rpc.rs:1109-1132`）：
```rust
agent.register_tool(Box::new(bash_run_tool));
agent.register_tool(Box::new(bash_kill_tool));
agent.register_tool(Box::new(bash_send_tool));
agent.register_tool(Box::new(bash_bg_tool));
// ... 其他手动注册
```

**改为**：删掉这些手动注册，在扩展注册完成后，遍历扩展调 `register_tools`。

在 worker 启动流程里（扩展都 register 进 ExtensionRunner 之后），加：
```rust
// Let each extension register its own tools (self-describing).
for ext in ext_reg.iter_extensions() {
    ext.register_tools(&mut tools);  // tools = ToolRegistry
}
```

**注意**：需要确认 ExtensionRunner 有没有 `iter_extensions()` 或类似遍历方法。如果没有，加一个（`src/agent/extension.rs` 的 ExtensionRunner impl 里）。实现前先读 ExtensionRunner 的结构（约行 689-799）。

同理 `src/bin/ion.rs` 的 `build_tools`（约行 1058-1152）如果有手动注册扩展工具，也改成遍历。

### 改动 4：export 重建时遍历扩展（`src/export.rs`）

**当前**（今天的临时修复）：`register_builtins()` 后手动补 SkillTool + else 分支（行 167-262）。

**改为**：
1. 保留 `register_builtins()`
2. 删掉手动 SkillTool 注册 + else 分支
3. 改为：构造一个临时 ExtensionRunner，注册所有需要的扩展，然后遍历调 `register_tools`

但 export 时不跑真实 worker，没有完整的扩展实例化环境。**简化方案**：export 专门建一个 helper，手动实例化 BashExtension + SkillTool（跟 worker 一样但用 dummy storage），调它们的 register_tools。

```rust
// src/export.rs，替换行 166-262 的手动注册逻辑：
let mut registry = crate::agent::tool::ToolRegistry::new();
registry.register_builtins();

// Register SkillTool (needs skill_dirs)
registry.register(Box::new(crate::agent::tool::SkillTool {
    skill_dirs,
    disabled: crate::config::IonConfig::load().skills.disabled,
}));

// Let BashExtension register bash_run/bash_kill/bash_send/bash_bg
let bash_ext = crate::agent::bash::BashExtension::new_for_export();
bash_ext.register_tools(&mut registry);
```

**注意**：BashExtension::new 需要 storage 参数（真实 worker 传 StorageContext）。export 时加一个 `new_for_export()` 或 `new_default()` 构造 dummy 实例（register_tools 只 clone Arc，不实际执行命令，dummy storage 够用）。实现前读 BashExtension::new 签名。

### 改动 5：其他扩展的 register_tools（可选，本次只做 bash）

LspExtension/GoalSupervisorExtension/PlanExtension/MCP/WASM 也可以加 register_tools，但本次**只做 BashExtension + SkillTool**（解决今天的问题）。其他扩展留 TODO，逐步迁移。

## 验证

```bash
# 1. cargo build 无 error/warning
cargo build 2>&1 | grep -E "error|warning|Finished"

# 2. export 工具面板含 bash_run（之前缺）
SID=<任意 build agent session>
target/debug/ion --export /tmp/test.html --session "$SID"
python3 -c "
import base64,json,re
html=open('/tmp/test.html').read()
m=re.search(r'<script id=\"session-data\"[^>]*>(.*?)</script>',html,re.DOTALL)
data=json.loads(base64.b64decode(m.group(1)).decode())
tools=sorted(t['name'] for t in data.get('tools',[]))
print('工具数:', len(tools))
print('含 skill:', 'skill' in tools)
print('含 bash_run:', 'bash_run' in tools)
print('含 bash_kill:', 'bash_kill' in tools)
"

# 3. worker 正常启动（bash_run 工具仍可用）
target/debug/ion --profile permissive "用 bash_run 跑 echo hello" 2>&1 | tail -3

# 4. lib 测试通过
cargo test --lib 2>&1 | grep "test result"
```

## 注意事项

1. ALL COMMENTS IN ENGLISH ONLY
2. 不改 BashRunTool 等工具的 struct 定义（只改注册位置）
3. 不改 ToolRegistry 的接口（只加调用方）
4. ExtensionRunner 如果没有 iter_extensions，加一个（返回 `Vec<&dyn Extension>` 或 `impl Iterator`）
5. BashExtension::new_for_export 是 dummy 实例，register_tools 只 clone Arc 不执行命令，安全
6. 删掉 worker_rpc.rs:1109-1132 的手动注册后，确认没有其他地方依赖那些局部变量（bash_run_tool 等）

## 改动清单

- [ ] 改动 1: Extension trait 加 register_tools 默认空方法（extension.rs）
- [ ] 改动 2: BashExtension impl register_tools（bash.rs）
- [ ] 改动 3: worker 启动遍历扩展注册（worker_rpc.rs + ion.rs，删手动注册）
- [ ] 改动 4: export 遍历扩展注册（export.rs，删手动 SkillTool + else 分支）
- [ ] ExtensionRunner 加 iter_extensions（如果缺）
- [ ] BashExtension 加 new_for_export（dummy 构造）
- [ ] cargo build + cargo test --lib 通过
- [ ] 命令行验证：export 含 bash_run + skill；worker bash_run 可用
