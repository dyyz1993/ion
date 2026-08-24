# hello-extension 手册

> **状态：已验证** — 最小 ABI、工具注册、工具执行和 `extension_on_input` 示例已实现。
>
> **类型：** 运行时 WASM Extension
> **Extension ID：** `hello_extension`
> **ABI 版本：** `1`
> **发行版本：** `0.1.0`

## 能力与边界

用于验证 Extension 加载、`host_register_tool`、工具执行和可变生命周期钩子的最小示例，不承载业务状态。

## 构建与安装

```bash
cd extensions/hello-extension
cargo build --target wasm32-wasip1 --release
ion extension install ./target/wasm32-wasip1/release/hello_extension.wasm
```

## 工具

| 名称 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `hello` | `{}` | `{"greeting":"Hello from extension!"}` | 验证工具调用链路 |

## 生命周期

`extension_on_input` 接收输入 JSON 并原样写回，用于演示 A 类可变钩子 ABI。

## 存储、RPC 与事件

本示例不使用持久化存储、不实现 Extension RPC，也不发射业务事件。

## 验证

```bash
cargo test --test wasm_extension_tests hello_extension_loads_and_registers_tool
```
