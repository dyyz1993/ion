# Extension 开发与接入工作流

> **状态：已验证** — 本文是 ION 运行时 WASM Extension 的唯一开发入口；ABI 细节以 `docs/design/EXTENSION_SYSTEM.md` 和加载器实现为准。

ION 只有两类 Extension：编译进内核的内置 Extension，以及运行时加载的 WASM Extension。第三方能力应实现为 WASM Extension；基础设施能力应进入内核，再通过 host functions 暴露给 Extension。

## 1. 最短闭环

```text
ion extension create
  -> 编写 Extension
  -> wasm32-wasip1 release build
  -> ion extension install 或项目级复制
  -> extension_list 确认已加载
  -> call_tool / extension_rpc 直调
  -> subscribe 验证事件
  -> Harness + ignored e2e + CLI CI
```

## 2. 创建 Extension

```bash
ion extension create hello-extension
cd hello-extension
```

脚手架包含三个基础 ABI 入口：

- `extension_version() -> u32`：当前 ABI 版本，必须返回 `1`
- `extension_init()`：注册工具并初始化状态
- `extension_execute_tool(...) -> u32`：处理工具调用并把 JSON 写入输出缓冲区

生命周期钩子统一使用 `extension_` 前缀，例如：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn extension_on_input(
    json_ptr: u32,
    json_len: u32,
    out_buf: u32,
    out_capacity: u32,
) -> u32 {
    // 返回 0 表示不修改输入；返回正数表示写入了新的 JSON。
    0
}
```

完整 ABI、钩子分类和 host functions 见 [Extension 系统设计](../design/EXTENSION_SYSTEM.md)。不要声明 `plugin_*` 符号。

## 3. 构建

每个示例 Extension 当前都是独立 Cargo workspace，因此在它自己的目录内构建：

```bash
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```

产物位于：

```text
target/wasm32-wasip1/release/<crate_name>.wasm
```

不要使用已经废弃的 `wasm32-wasi`，也不要使用与当前 host ABI 不匹配的 `wasm32-unknown-unknown`。

## 4. 安装与加载

### 全局安装

```bash
ion extension install ./target/wasm32-wasip1/release/hello_extension.wasm
ion extension list
```

全局目录是 `~/.ion/agent/extensions/`，会被所有项目自动发现。

### 项目级安装

```bash
mkdir -p <project>/.ion/extensions
cp ./target/wasm32-wasip1/release/hello_extension.wasm \
  <project>/.ion/extensions/
```

项目目录只对当前项目生效。`--no-extensions` 可以关闭自动发现。

### 单次加载或运行时热加载

```bash
# 单次 CLI 进程加载
ion --extension /absolute/path/to/hello_extension.wasm "调用 hello 工具"

# 已有 session 中热加载
ion rpc --session <sid> --method extension_add \
  --params '{"path":"/absolute/path/to/hello_extension.wasm"}'
```

术语约定：

- **installed**：`.wasm` 已存在于全局或项目扩展目录
- **loaded**：当前 Worker 已实例化该 Extension
- **enabled**：配置允许自动发现或内置 Extension 启用

`ion extension list` 查看已安装产物；RPC `extension_list` 查看当前 Worker 已加载实例。

## 5. 不经过 LLM 的核心验证

先验证 RPC/运行时闭环，再验证 LLM 是否会自主选择工具。

```bash
# 创建 session
ion rpc --method create_session --params '{"agent":"developer"}'

# 查看当前 Worker 真正加载的 WASM Extension
ion rpc --session <sid> --method extension_list

# 直接调用工具
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"hello","args":{}}'

# 调用 Extension 私有 RPC
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"hello_extension","method":"status","params":{}}'

# 订阅 Extension 事件
ion subscribe --session <sid> --extension hello_extension
```

至少验证：正常参数、错误参数、未知工具、输出缓冲区边界、重启后的持久化和多终端事件同步。

## 6. 存储维度

| 维度 | Host functions | 用途 |
|------|----------------|------|
| `global` | `host_{read,write,delete,list}_global_data` | 所有项目共享的本机数据 |
| `project` | `host_{read,write,delete,list}_project_data` | 项目私有但不提交 Git 的数据 |
| `project_local` | `host_{read,write,delete,list}_project_local_data` | 项目目录中的可移植数据 |
| `session` | `host_{read,write,delete,list}_session_data` | 单 session 数据 |

路径必须由内核计算；Extension 不应自行拼接 `~/.ion` 路径。文件能力和安全边界见 [Extension Host API](../design/EXTENSION_HOST_API.md)。

## 7. 必须完成的验证

每个新 Extension 都要具备：

1. FauxProvider Factory 驱动的 Harness 集成测试。
2. `#[ignore]` 真实 LLM case，由 `ION_E2E=1` 触发。
3. `tests/<extension>_ci.sh`，从命令行启动 host、调用 RPC、订阅事件并断言。
4. 源码目录下的 `MANUAL.md`，使用 [Extension 手册模板](../templates/EXTENSION_MANUAL_TEMPLATE.md)。
5. Push、Pull、多终端同步三个对外能力；不适用时在 MANUAL 中说明原因。

CI 脚本必须保存启动进程的精确 PID 并按 PID 清理，禁止使用宽泛的 `pkill -f "ion"`。

## 8. 参考实现

- [`extensions/hello-extension/`](../../extensions/hello-extension/)：最小工具注册与调用示例
- [`extensions/todo-extension/`](../../extensions/todo-extension/)：带 session 存储、MANUAL 和 host 集成测试的完整示例
- [`extensions/file-time-guard/`](../../extensions/file-time-guard/)：生命周期钩子与 `extension_on_rpc` 示例
- [`extensions/session-supervisor/`](../../extensions/session-supervisor/)：Agent 生命周期与主动 steer 示例

设计决策看 `EXTENSION_SYSTEM.md`，具体扩展的使用方式只看该扩展自己的 `MANUAL.md`。
