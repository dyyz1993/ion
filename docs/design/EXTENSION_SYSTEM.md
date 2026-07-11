# ION 扩展系统

> **状态：已完成** — WASM 扩展加载、热更新、数据存储四大维度已实现并验证。

## 概述

ION 的扩展系统分两层：

```
┌─────────────────────────────────────────────────────┐
│  第一层：Rust 扩展（Extension trait）                │
│  29 个钩子，对齐 PI 事件体系                          │
│  语言：Rust 原生                                     │
│  文件：src/agent/extension.rs                        │
├─────────────────────────────────────────────────────┤
│  第二层：WASM 扩展                                    │
│  wasmtime 沙箱隔离，多语言（编译到 wasm32-wasip1）     │
│  注册工具 + 热更新 + 4 维数据存储                      │
│  文件：src/wasm_extension.rs                                  │
└─────────────────────────────────────────────────────┘
```

本文档聚焦第二层（WASM 扩展）。Rust 扩展层见 `src/agent/extension.rs` 及 `Extension` trait 的 29 个方法签名。

---

## WASM 扩展生命周期

```
┌─────────┐    ┌──────────┐    ┌──────────────┐    ┌──────────┐
│ Load    │───→│ Init     │───→│ Execute Tool  │───→│ Unload   │
│ 编译    │    │ 注册工具  │    │ 被 LLM 调用   │    │ drop     │
│ 分配内存 │    │ 恢复状态  │    │ 访问 4 维存储  │    │ 释放内存  │
└─────────┘    └──────────┘    └──────────────┘    └──────────┘
```

### 扩展导出的 3 个 C 函数

```rust
// 必须导出

#[no_mangle]
pub extern "C" fn extension_version() -> u32
    // 返回版本号，目前仅做日志记录

#[no_mangle]
pub extern "C" fn extension_init()
    // 在这里注册工具、恢复状态
    // 可调用宿主函数：host_register_tool、host_read_*_data

#[no_mangle]
pub extern "C" fn extension_execute_tool(
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

## 热更新（ExtensionRegistry）

### 架构

```rust
// src/wasm_extension.rs
pub struct ExtensionRegistry {
    extensions: RwLock<HashMap<String, ExtensionEntry>>,
    pub ctx: RwLock<ExtensionContext>,
}
```

每个 `WasmCallingTool` 持有 `registry + extension_path` 引用，不直接持有 `WasmExtension` 实例：

```
WasmCallingTool ─→ ExtensionRegistry ─→ HashMap<path, Arc<Mutex<WasmExtension>>>
                                                            ↕
                                                     LLM 调用时临时持有
```

**核心原理：** 当 `extension_remove` 时，旧的 `Arc<Mutex<WasmExtension>>` 从 HashMap 移除。如果此时没有 tool 正在执行，Arc 降为 0，`WasmExtension` 及其 `wasmtime::Store` 立即释放。如有 tool 正在执行，该 tool 的执行闭包持有临时的 Arc.clone()，执行完即释放。

### 对比 PI

| 方面 | ION | PI |
|------|-----|----|
| reload 粒度 | **扩展级**（不重建会话） | 全量会话重建（shutdown → reload → session_start） |
| 注册/反注册 | `registerProvider` / `unregisterProvider`（runner.ts:460） | `extension_add` / `extension_remove` |
| 即时生效 | ✅ 命令间天然串行，修改即时生效 | ✅ same |
| 状态持久化 | 通过 4 维数据宿主函数（扩展自主读写） | 通过 `getSessionDataDir` 等路径 API |

### RPC 命令

```
extension_add     加载一个 .wasm 文件到注册表
extension_remove  卸载并释放扩展
extension_reload  重新加载（移除旧 + 添加新）
extension_list    列出所有已加载的扩展及工具
reload         通用 reload（遍历所有已加载扩展执行 extension_reload）
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
- **不需要文件锁：** `Mutex<WasmExtension>` 保证同一进程内串行化；跨进程的 `rename` 是 POSIX 原子操作。

### WASM 扩展的典型用法

```rust
// todo-plugin 的伪代码：带持久化的 todo 工具

#[no_mangle]
pub extern "C" fn extension_init() {
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

## 上下文注入（ExtensionContext）

工具执行前，宿主读取 `ExtensionRegistry.ctx` + `WasmCallingTool.ext_name`，合并为 `ExtensionContext` 注入到 WASM Store。

```rust
pub struct ExtensionContext {
    pub session_id: String,    // 当前会话 ID
    pub cwd: String,           // 当前工作目录
    pub project_root: String,  // 项目根目录
    pub ext_name: String,      // 扩展扩展名
}
```

注入时机：
- **CLI 模式：** `agent.run()` 前设置 `registry.ctx`
- **Worker 模式：** `prompt` RPC 命令处理时设置 `registry.ctx`

---

## 现有扩展

| 扩展 | 目录 | 工具 | 特点 |
|------|------|------|------|
| **stock-plugin** | `stock-plugin/` | `get_stock_price` | `#![no_std]` 教学示例，演示 channel_send |
| **plan-plugin** | `plan-plugin/` | `plan_enter`, `plan_exit` | 配合 Rust 端 `PlanExtension` 使用 |
| **todo-plugin** | `todo-plugin/` | `todo_create`, `todo_update`, `todo_list` | 唯一有内存状态（`static mut ITEMS`）的示例 |

---

## 如何写一个 WASM 扩展

### 1. 创建项目

```toml
# Cargo.toml
[package]
name = "my-extension"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

# 可选：需要 JSON 序列化时
# [dependencies]
# serde_json = { version = "1", default-features = false, features = ["alloc"] }
```

### 2. 编写扩展

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
pub extern "C" fn extension_version() -> u32 { 1 }

#[no_mangle]
pub extern "C" fn extension_init() {
    host_register(
        "my_tool",
        "Description of the tool",
        r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#,
    );
}

#[no_mangle]
pub extern "C" fn extension_execute_tool(
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
ion --extension ./target/wasm32-wasip1/release/my_extension.wasm "帮我执行 my_tool"

# Worker 模式 — 运行中热加载
echo '{"id":"1","method":"extension_add","params":{"path":"/abs/to/my_extension.wasm"}}' |
  ion-worker --mode rpc
```

---

## 文件参考

| 文件 | 说明 |
|------|------|
| `src/wasm_extension.rs` | WASM 扩展加载器、ExtensionRegistry、16 个宿主函数 |
| `src/agent/extension.rs` | Rust 扩展 Extension trait（29 钩子） |
| `src/worker_api.rs` | ExtensionApi（create_worker、channel_send 等） |
| `src/paths.rs` | 数据存储路径定义（global/project/session/project_local） |
| `stock-plugin/src/lib.rs` | 最简单的 WASM 扩展示例 |
| `todo-plugin/src/lib.rs` | 有状态的 WASM 扩展示例 |
| `plan-plugin/src/lib.rs` | PRD 计划模式 WASM 扩展 |
| `src/bin/ion_worker.rs` | RPC 命令 dispatch（含 extension_add/remove/list/reload） |
| `tests/plugin_tests.rs` | 27 个扩展测试 |

---

## 11. Flags 系统（运行时扩展 flag 读写）

### 11.1 概述

扩展可以在 JSON 配置里声明 flag（`FlagDef`），用户运行时通过 RPC 读写 flag 值来调整扩展行为。

| 能力 | 入口 | 说明 |
|------|------|------|
| 声明 flag（静态） | 扩展 JSON 配置 | `flags: { verbose: { description, type, default } }` |
| 读 flag（运行时） | `get_flags` RPC | 返回运行时值（优先）或 default |
| 写 flag（运行时） | `set_flag` RPC | 修改运行时值（session 内存） |
| 扩展内读 flag | `ExtensionRegistry::get_flag()` | 扩展代码内部查当前值 |

### 11.2 `get_flags` RPC 接口规格

**请求：**

```bash
# 查所有扩展的 flag
ion rpc --session <sid> --method get_flags

# 查指定扩展的 flag
ion rpc --session <sid> --method get_flags \
  --params '{"extension":"my-ext"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `extension` | string | 可选 | 扩展名。省略 = 返回所有扩展 |

**响应 JSON（成功，指定扩展）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "get_flags",
  "success": true,
  "data": {
    "extension": "my-ext",
    "flags": {
      "verbose": false,
      "max_items": 100
    }
  }
}
```

**响应 JSON（成功，全部扩展）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "get_flags",
  "success": true,
  "data": {
    "memory": {},
    "bash": {}
  }
}
```

**响应 JSON（失败）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "get_flags",
  "success": true,
  "data": {
    "extension": "nonexistent",
    "flags": {}
  }
}
```

> 未设值的扩展返回空 `{}`（无运行时 flag 时为空对象）。

### 11.3 `set_flag` RPC 接口规格

**请求：**

```bash
ion rpc --session <sid> --method set_flag \
  --params '{"extension":"my-ext","flag":"verbose","value":true}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `extension` | string | 必填 | 扩展名 |
| `flag` | string | 必填 | flag 名 |
| `value` | any | 必填 | flag 值（bool/number/string/object/array） |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "set_flag",
  "success": true,
  "data": {
    "extension": "my-ext",
    "flag": "verbose",
    "value": true,
    "set": true
  }
}
```

**响应 JSON（失败，缺参数）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "set_flag",
  "success": true,
  "data": {
    "error": "missing 'extension' or 'flag' parameter"
  }
}
```

### 11.4 CLI 测试指南

#### Group F: Flags 系统

##### F1 查询所有扩展的 flag

```bash
ion rpc --session sess_xxx --method get_flags
```

**验证点：**
- ✅ 返回所有已加载扩展的 flag（可能为空对象）
- ✅ 不崩溃

##### F2 设置 flag

```bash
ion rpc --session sess_xxx --method set_flag \
  --params '{"extension":"memory","flag":"debug","value":true}'
```

**验证点：**
- ✅ 返回 `set: true`
- ✅ value 反映设置的值

##### F3 设置后查询

```bash
# 先 set
ion rpc --session sess_xxx --method set_flag \
  --params '{"extension":"memory","flag":"debug","value":true}'

# 再 get
ion rpc --session sess_xxx --method get_flags \
  --params '{"extension":"memory"}'
```

**验证点：**
- ✅ get 返回的 flags 含 `debug: true`
- ✅ 值与 set 的一致

##### F4 缺参数报错

```bash
ion rpc --session sess_xxx --method set_flag \
  --params '{"flag":"debug","value":true}'
```

**验证点：**
- ✅ 返回 error "missing 'extension' or 'flag'"

##### F5 查不存在的扩展

```bash
ion rpc --session sess_xxx --method get_flags \
  --params '{"extension":"nonexistent"}'
```

**验证点：**
- ✅ 返回空 `flags: {}`，不崩溃

##### F6 设置不同类型的 flag 值

```bash
ion rpc --session sess_xxx --method set_flag \
  --params '{"extension":"memory","flag":"limit","value":42}'

ion rpc --session sess_xxx --method set_flag \
  --params '{"extension":"memory","flag":"mode","value":"strict"}'
```

**验证点：**
- ✅ number 类型正常
- ✅ string 类型正常
