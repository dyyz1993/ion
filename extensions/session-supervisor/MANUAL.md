# session-supervisor 手册

> **状态：开发中** — Agent 结束检查、主动 steer 与 Extension RPC 已实现，CLI CI 和真实 case 待补齐。
>
> **类型：** 运行时 WASM Extension
> **Extension ID：** `session_supervisor_wasm`
> **ABI 版本：** `1`
> **发行版本：** `0.1.0`

## 能力与边界

在 `extension_on_agent_end` 中检查最后一条回复是否为空、是否使用过工具，以及是否残留 TODO/FIXME/WIP 等标记；发现问题时通过 `host_steer` 要求 Agent 继续修正。

## 构建与安装

```bash
cd extensions/session-supervisor
cargo build --target wasm32-wasip1 --release
ion extension install ./target/wasm32-wasip1/release/session_supervisor_wasm.wasm
```

## Extension RPC

| method | 说明 |
|--------|------|
| `status` | 返回最近一次监督结果 |
| `check` | 立即检查当前消息历史 |

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"session_supervisor_wasm","method":"status","params":{}}'
```

## 存储与事件

最近一次结果保存在 Extension 进程内存中；当前通过 steer 改变执行，不发射独立业务事件。

## 验证

需要补 FauxProvider Factory Harness、`tests/session_supervisor_ci.sh` 和 `#[ignore]` 真实 case。
