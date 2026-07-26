# Monitor Scheduler — Singleton Extension 模式

## 架构

```
ion serve 启动
  └─ WorkerRegistry.init_singletons()
       └─ MonitorExtension::on_singleton_post_init(registry)
            ├─ 加载 .ion/monitors/*.json
            └─ 每个 monitor: tokio::spawn(interval loop)
                 ├─ 每 N 秒运行 bash 脚本
                 ├─ stdout 非空 → 有事件
                 └─ 有事件 → registry.send_command(worker_id, "prompt", ...)
```

## 改动清单（2 新文件 + 2 处注册）

### 1. `src/monitor_extension.rs`（~200 行）

- `MonitorDef`：name / interval_secs / script / agent / prompt_template / enabled
- `MonitorExtension`：singleton，`on_singleton_post_init` 里 spawn interval loop
- interval loop：运行脚本 → 判断 stdout → 触发 LLM
- `on_extension_rpc`：list / add / remove / enable / disable / status

### 2. `src/lib.rs` 加 `pub mod monitor_extension;`

### 3. `src/bin/ion_worker.rs` 注册：
```rust
if ion_cfg.is_extension_enabled("monitor") {
    ext_reg.register(Box::new(ion::monitor_extension::MonitorExtension::new()));
}
```

### 4. Monitor 定义文件格式（`.ion/monitors/*.json`）
```json
{
  "name": "github-issues",
  "interval_secs": 300,
  "script": "gh issue list --label bug 2>/dev/null",
  "agent": "developer", 
  "prompt_template": "New issues:\n{output}\nProcess them.",
  "enabled": true
}
```

## 运行时行为

- 脚本 exit=0 + stdout 空 → 没事，继续循环
- 脚本 exit=0 + stdout 非空 → 有事件，触发 LLM 对话
- 脚本 exit≠0 → 记录错误，继续循环
- Agent 可以通过 extension_rpc 自己 add/remove monitor（自生成脚本）

## 只在 serve/--host 模式下注册

跟 GlobalMemoryExtension 一样，只在 `ion_worker.rs` 的 serve 路径注册。场景 1 不注册。

## 不改的东西

- 不改 Extension trait（不加 on_tick）
- 不改 hooks 系统
- 不改 Cargo.toml
- 不改 agent_loop