# file-time-guard 手册

> **状态：开发中** — 文件时间标记、tool hook 与 Extension RPC 已实现，CLI CI 和真实 case 待补齐。
>
> **类型：** 运行时 WASM Extension
> **Extension ID：** `file_time_guard_wasm`
> **ABI 版本：** `1`
> **发行版本：** `0.1.0`

## 能力与边界

跟踪 `write` / `edit` 工具涉及文件的修改时间标记，供调用方检查文件是否可能在 Agent 读取后被外部修改。

## 构建与安装

```bash
cd extensions/file-time-guard
cargo build --target wasm32-wasip1 --release
ion extension install ./target/wasm32-wasip1/release/file_time_guard_wasm.wasm
```

## Extension RPC

| method | 说明 |
|--------|------|
| `status` | 返回当前跟踪状态 |
| `check` | 检查指定文件是否过期 |
| `reset` | 清除跟踪状态 |

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"file_time_guard_wasm","method":"status","params":{}}'
```

## 存储与事件

状态保存在 Extension 进程内存中，Worker 重启后重置；当前没有业务事件 Push。

## 验证

需要补 `tests/file_time_guard_ci.sh`、FauxProvider Harness 和 `#[ignore]` 真实 case。
