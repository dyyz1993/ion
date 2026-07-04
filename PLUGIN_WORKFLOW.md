# 插件开发与测试工作流

> **状态：已验证** — todo-plugin 已完整走通此流程。

## 开发测试闭环

```
┌──────────────────────────────────────────────────────────────┐
│  1. 写代码                                    todo-plugin/   │
│     └── #![no_std] WASM crate + host fn 声明                 │
├──────────────────────────────────────────────────────────────┤
│  2. Build                                     cargo build    │
│     └── wasm32-wasip1 release → todo_plugin.wasm             │
├──────────────────────────────────────────────────────────────┤
│  3. 安装                                       cp *.wasm     │
│     └── {project}/.ion/extensions/  或  ~/.ion/agent/ext/    │
├──────────────────────────────────────────────────────────────┤
│  4. RPC 直调验证（不经过 LLM）                    call_tool   │
│     └── 快速验证每个工具的正常/异常路径                       │
├──────────────────────────────────────────────────────────────┤
│  5. LLM 引导调用                               prompt        │
│     └── LLM 自主识别工具、参数并调用                          │
├──────────────────────────────────────────────────────────────┤
│  6. RPC 佐证验证                               call_tool     │
│     └── 查数据是否按预期持久化                                │
│     └── 遍历所有工具组合                                      │
└──────────────────────────────────────────────────────────────┘
```

## 1. 插件源码结构

```
ion/todo-plugin/                  ← 插件 crate 目录
├── Cargo.toml                    ← crate-type = ["cdylib"]
└── src/
    └── lib.rs                    ← #![no_std], WASM 入口
```

**要点：**
- `#![no_std]` — WASM 环境无标准库
- `#[panic_handler]` — 必须提供
- 宿主函数用 `extern "C"` 声明（详见 [PLUGIN_SYSTEM.md](./PLUGIN_SYSTEM.md)）
- `plugin_version()` / `plugin_init()` / `plugin_execute_tool()` — 三个入口函数

## 2. Build

```bash
cd ion && cargo build --target wasm32-wasip1 --release -p <crate-name>
# 产物：target/wasm32-wasip1/release/<name>.wasm
```

**添加到 workspace**（如果尚未）：
```toml
# ion/Cargo.toml
[workspace]
members = ["<crate-name>"]
```

## 3. 安装路径

| 级别 | 路径 | 作用域 |
|------|------|--------|
| **项目级** | `<project>/.ion/extensions/<name>.wasm` | 当前项目 |
| **全局** | `~/.ion/agent/extensions/<name>.wasm` | 所有项目 |

**自动发现**：启动时扫描以上两个目录的 `*.wasm`。`--no-extensions` 禁用。

```bash
cp target/wasm32-wasip1/release/todo_plugin.wasm <project>/.ion/extensions/
# 重启 Manager 或 reload 即可加载
```

## 4. 插件数据存储（4 维）

数据通过宿主函数读写，路径由内核管理：

| 维度 | 宿主函数 | 存储路径 |
|------|---------|---------|
| **session** | `host_read/write/delete/list_session_data` | `~/.ion/agent/sessions/{cwd_hash}/data/{sid}/{ext_name}/{key}` |
| **project** | `host_read/write/delete/list_project_data` | `~/.ion/agent/project-data/{hash}--{name}/{ext_name}/{key}` |
| **global** | `host_read/write/delete/list_global_data` | `~/.ion/agent/extensions-data/{ext_name}/{key}` |
| **project_local** | `host_read/write/delete/list_project_local_data` | `~/.ion/agent/tmp/extensions/{ext_name}/{key}` |

**持久化保证**：
- session 维度：session 不删数据就在，Manager/Worker 重启不丢
- project 维度：项目路径不变数据就在
- global 维度：全局数据，所有项目可见
- project_local：临时数据，可回收

详见 [PLUGIN_SYSTEM.md](./PLUGIN_SYSTEM.md) 的宿主函数签名。

## 5. 测试工作流（核心）

### 5.1 RPC 直调验证

不经过 LLM，直接触发工具，验证返回值：

```bash
# 启动 Manager
ion manager start

# 创建 session
ion rpc --method create_session --params '{"agent":"developer"}'
# → sess_xxx

# 直调每个工具
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_add","args":{"text":"测试任务"}}'
# → {"id":"1","text":"测试任务","status":"created"}

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"all"}}'
# → [{"id":"1","text":"测试任务","done":false}]

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_done","args":{"id":"1"}}'
# → {"id":"1","status":"done"}

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_remove","args":{"id":"1"}}'
# → {"id":"1","status":"removed"}
```

**验证点**：返回值 JSON 格式正确、数据写入存储、错误路径有合理错误信息。

### 5.2 LLM 引导调用

让 LLM 自主使用工具：

```bash
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"请用 todo_add 创建三个任务：A、B、C"}'
# → LLM 会识别的 todo_add 工具并调用
```

**验证点**：LLM 能正确识别工具名、参数 JSON schema、在合适的上下文调用。

### 5.3 RPC 佐证

LLM 调用后，用 RPC 查存储确认：

```bash
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"all"}}'
# → 确认 3 个任务都已创建

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"active"}}'
# → 确认 done:false 的过滤正确
```

**验证点**：LLM 操作确实写入了 session 存储、数据格式正确、持久化正常。

### 5.4 持久化验证

```bash
# 关 Manager
kill $(cat ~/.ion/manager.pid)

# 重启
ion manager start

# 数据还在
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"all"}}'
# → 之前的数据仍在
```

## 6. 完整验证清单

| 阶段 | 操作 | 验证项 |
|------|------|--------|
| RPC 直调 | 每个工具调一次 | 返回 JSON 格式、字段名、类型 |
| RPC 直调 | 错误参数 | 错误信息合理 |
| LLM 引导 | prompt 描述场景 | LLM 正确选工具、填参数 |
| RPC 佐证 | 查存储状态 | 数据写入正确 |
| 持久化 | 关 → 开 Manager | 数据不丢失 |
| 遍历 | 所有工具组合 | create → done → list → remove → clean |

## 7. 参考

- [PLUGIN_SYSTEM.md](./PLUGIN_SYSTEM.md) — WASM 宿主函数、热更新、4D 存储实现
- [stock-plugin/](./stock-plugin/) — 最小 WASM 插件示例
- [todo-plugin/](./todo-plugin/) — 完整 TODO 插件（session 维度）
