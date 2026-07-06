# ION 插件系统

> **状态：已完成** — WASM 插件加载、热更新、数据存储四大维度已实现并验证。

## 概述

ION 的插件系统分两层：

```
┌─────────────────────────────────────────────────────┐
│  第一层：Rust 扩展（Extension trait）                │
│  29 个钩子，对齐 PI 事件体系                          │
│  语言：Rust 原生                                     │
│  文件：src/agent/extension.rs                        │
├─────────────────────────────────────────────────────┤
│  第二层：WASM 插件                                    │
│  wasmtime 沙箱隔离，多语言（编译到 wasm32-wasip1）     │
│  注册工具 + 热更新 + 4 维数据存储                      │
│  文件：src/plugin.rs                                  │
└─────────────────────────────────────────────────────┘
```

本文档聚焦第二层（WASM 插件）。Rust 扩展层见 `src/agent/extension.rs` 及 `Extension` trait 的 29 个方法签名。

---

## WASM 插件生命周期

```
┌─────────┐    ┌──────────┐    ┌──────────────┐    ┌──────────┐
│ Load    │───→│ Init     │───→│ Execute Tool  │───→│ Unload   │
│ 编译    │    │ 注册工具  │    │ 被 LLM 调用   │    │ drop     │
│ 分配内存 │    │ 恢复状态  │    │ 访问 4 维存储  │    │ 释放内存  │
└─────────┘    └──────────┘    └──────────────┘    └──────────┘
```

### 插件导出的 3 个 C 函数

```rust
// 必须导出

#[no_mangle]
pub extern "C" fn plugin_version() -> u32
    // 返回版本号，目前仅做日志记录

#[no_mangle]
pub extern "C" fn plugin_init()
    // 在这里注册工具、恢复状态
    // 可调用宿主函数：host_register_tool、host_read_*_data

#[no_mangle]
pub extern "C" fn plugin_execute_tool(
    name_ptr: *const u8,  name_len: u32,      // 工具名
    args_ptr: *const u8,  args_len: u32,       // JSON 参数
    out_buf: *mut u8,     out_capacity: u32,    // 输出缓冲区
) -> u32                                         // 返回输出长度
```

### 宿主提供的 C 函数

| 函数 | 说明 |
|------|------|
| `host_register_tool(name, desc, schema)` | 注册一个 LLM 可调用的工具 |
| `host_send_message(text)` | 发送消息到用户事件流 |
| `host_channel_send(channel, msg)` | 广播到命名频道 |
| `host_create_worker(config)` | 请求创建子 Worker |

### 数据存储函数（16 个，见下文）

---

## 热更新（PluginRegistry）

### 架构

```rust
// src/plugin.rs
pub struct PluginRegistry {
    plugins: RwLock<HashMap<String, PluginEntry>>,
    pub ctx: RwLock<PluginContext>,
}
```

每个 `WasmCallingTool` 持有 `registry + plugin_path` 引用，不直接持有 `WasmPlugin` 实例：

```
WasmCallingTool ─→ PluginRegistry ─→ HashMap<path, Arc<Mutex<WasmPlugin>>>
                                                            ↕
                                                     LLM 调用时临时持有
```

**核心原理：** 当 `extension_remove` 时，旧的 `Arc<Mutex<WasmPlugin>>` 从 HashMap 移除。如果此时没有 tool 正在执行，Arc 降为 0，`WasmPlugin` 及其 `wasmtime::Store` 立即释放。如有 tool 正在执行，该 tool 的执行闭包持有临时的 Arc.clone()，执行完即释放。

### 对比 PI

| 方面 | ION | PI |
|------|-----|----|
| reload 粒度 | **插件级**（不重建会话） | 全量会话重建（shutdown → reload → session_start） |
| 注册/反注册 | `registerProvider` / `unregisterProvider`（runner.ts:460） | `extension_add` / `extension_remove` |
| 即时生效 | ✅ 命令间天然串行，修改即时生效 | ✅ same |
| 状态持久化 | 通过 4 维数据宿主函数（插件自主读写） | 通过 `getSessionDataDir` 等路径 API |

### RPC 命令

```
extension_add     加载一个 .wasm 文件到注册表
extension_remove  卸载并释放插件
extension_reload  重新加载（移除旧 + 添加新）
extension_list    列出所有已加载的插件及工具
reload         通用 reload（遍历所有已加载插件执行 extension_reload）
```

---

## 数据存储四大维度

每个维度 4 个宿主函数（write / read / delete / list），共 16 个。

### 维度一览

| 维度 | 宿主函数前缀 | 目录 | 可 git 提交 | 跨机器共享 |
|------|------------|------|------------|-----------|
| **全局** | `global` | `~/.ion/agent/extensions-data/<extName>/` | ❌ | ❌ |
| **项目** (~/.ion) | `project` | `~/.ion/agent/project-data/<hash>/<extName>/` | ❌ | ❌ |
| **项目本地** | `project_local` | `<project_root>/.ion/<extName>/` | ✅ | ✅（需提交） |
| **会话** | `session` | `~/.ion/agent/sessions/<hash>/data/<sid>/<extName>/` | ❌ | ❌ |

### 宿主函数签名

```c
// 以 global 为例，其他维度同理（只换前缀名）

// 写入（原子化：先写 .tmp，再 rename）
// 返回 0 = 成功，1 = 失败
u32 host_write_global_data(
    key_ptr: u32,  key_len: u32,    // 文件名（key）
    data_ptr: u32, data_len: u32    // 数据内容
);

// 读取
// 返回字节数，0 = 不存在
u32 host_read_global_data(
    key_ptr: u32,    key_len: u32,     // 文件名
    out_buf: u32,    out_capacity: u32 // 输出缓冲区
);

// 删除
// 返回 0 = 成功，1 = 不存在
u32 host_delete_global_data(key_ptr: u32, key_len: u32);

// 列出所有文件（\n 分隔的文件名列表）
// 返回写入 out_buf 的字节数
u32 host_list_global_data(out_buf: u32, out_capacity: u32);
```

### 并发安全

- **原子写入：** 写入先写 `.tmp` 文件，再 `rename` 到目标路径。读取永远读不到半截数据。
- **冲突策略：** last write wins。多个 Worker 同时写同一 key，最后写入者生效。
- **不需要文件锁：** `Mutex<WasmPlugin>` 保证同一进程内串行化；跨进程的 `rename` 是 POSIX 原子操作。

### WASM 插件的典型用法

```rust
// todo-plugin 的伪代码：带持久化的 todo 工具

#[no_mangle]
pub extern "C" fn plugin_init() {
    // 注册工具（同前）
    host_register_tool(...);

    // 恢复状态：从会话级存储读取
    let mut buf = [0u8; 4096];
    let len = host_read_session_data(b"todos\0", &mut buf, 4096);
    if len > 0 {
        // 反序列化 buf → ITEMS
    }
}

fn save_state() {
    let json: &[u8] = ...;
    // 写会话级 + 项目本地级（双重备份）
    host_write_session_data(b"todos\0", json);
    host_write_project_local_data(b"todos\0", json);
}
```

---

## 上下文注入（PluginContext）

工具执行前，宿主读取 `PluginRegistry.ctx` + `WasmCallingTool.ext_name`，合并为 `PluginContext` 注入到 WASM Store。

```rust
pub struct PluginContext {
    pub session_id: String,    // 当前会话 ID
    pub cwd: String,           // 当前工作目录
    pub project_root: String,  // 项目根目录
    pub ext_name: String,      // 插件扩展名
}
```

注入时机：
- **CLI 模式：** `agent.run()` 前设置 `plugin_registry.ctx`
- **Worker 模式：** `prompt` RPC 命令处理时设置 `plugin_registry.ctx`

---

## 现有插件

| 插件 | 目录 | 工具 | 特点 |
|------|------|------|------|
| **stock-plugin** | `stock-plugin/` | `get_stock_price` | `#![no_std]` 教学示例，演示 channel_send |
| **plan-plugin** | `plan-plugin/` | `plan_enter`, `plan_exit` | 配合 Rust 端 `PlanExtension` 使用 |
| **todo-plugin** | `todo-plugin/` | `todo_create`, `todo_update`, `todo_list` | 唯一有内存状态（`static mut ITEMS`）的示例 |

---

## 如何写一个 WASM 插件

### 1. 创建项目

```toml
# Cargo.toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

# 可选：需要 JSON 序列化时
# [dependencies]
# serde_json = { version = "1", default-features = false, features = ["alloc"] }
```

### 2. 编写插件

```rust
#![no_std]

extern "C" {
    fn host_register_tool(
        name_ptr: *const u8, name_len: u32,
        desc_ptr: *const u8, desc_len: u32,
        schema_ptr: *const u8, schema_len: u32,
    );
    fn host_write_global_data(
        key_ptr: *const u8, key_len: u32,
        data_ptr: *const u8, data_len: u32,
    ) -> u32;
    fn host_read_global_data(
        key_ptr: *const u8, key_len: u32,
        out_buf: *mut u8, out_capacity: u32,
    ) -> u32;
}

fn host_register(name: &str, desc: &str, schema: &str) {
    unsafe {
        host_register_tool(
            name.as_ptr(), name.len() as u32,
            desc.as_ptr(), desc.len() as u32,
            schema.as_ptr(), schema.len() as u32,
        );
    }
}

#[no_mangle]
pub extern "C" fn plugin_version() -> u32 { 1 }

#[no_mangle]
pub extern "C" fn plugin_init() {
    host_register(
        "my_tool",
        "Description of the tool",
        r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#,
    );
}

#[no_mangle]
pub extern "C" fn plugin_execute_tool(
    _name_ptr: *const u8, _name_len: u32,
    _args_ptr: *const u8, _args_len: u32,
    out_buf: *mut u8, out_capacity: u32,
) -> u32 {
    let result = b"{\"status\":\"ok\",\"result\":\"hello from WASM\"}";
    let len = result.len().min(out_capacity as usize);
    unsafe { core::ptr::copy_nonoverlapping(result.as_ptr(), out_buf, len); }
    len as u32
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
```

### 3. 编译

```bash
cargo build --target wasm32-wasip1 --release
```

### 4. 运行

```bash
# CLI 模式
ion --extension ./target/wasm32-wasip1/release/my_plugin.wasm "帮我执行 my_tool"

# Worker 模式 — 运行中热加载
echo '{"id":"1","method":"extension_add","params":{"path":"/abs/to/my_plugin.wasm"}}' |
  ion-worker --mode rpc
```

---

## 文件参考

| 文件 | 说明 |
|------|------|
| `src/plugin.rs` | WASM 插件加载器、PluginRegistry、16 个宿主函数 |
| `src/agent/extension.rs` | Rust 扩展 Extension trait（29 钩子） |
| `src/worker_api.rs` | ExtensionApi（create_worker、channel_send 等） |
| `src/paths.rs` | 数据存储路径定义（global/project/session/project_local） |
| `stock-plugin/src/lib.rs` | 最简单的 WASM 插件示例 |
| `todo-plugin/src/lib.rs` | 有状态的 WASM 插件示例 |
| `plan-plugin/src/lib.rs` | PRD 计划模式 WASM 插件 |
| `src/bin/ion_worker.rs` | RPC 命令 dispatch（含 extension_add/remove/list/reload） |
| `tests/plugin_tests.rs` | 27 个插件测试 |
