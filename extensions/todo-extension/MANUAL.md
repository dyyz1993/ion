# todo-extension Manual

> **状态：已验证** — 工具、session 存储和 host 集成测试已覆盖。
>
> **类型：** 运行时 WASM Extension
>
> **Extension ID：** `todo_extension`
>
> **ABI 版本：** `1`
>
> **发行版本：** `0.1.0`

## Capabilities

- Manage todo tasks (add / list / mark done / remove / clean)
- Data is persisted per session

## Tools

| Name | Params | Description |
|------|--------|-------------|
| `todo_add` | `{text}` | Create a task |
| `todo_list` | `{status?}` | List tasks (all/active/done) |
| `todo_done` | `{id}` | Mark a task done |
| `todo_remove` | `{id}` | Delete a task |
| `todo_clean` | `{}` | Remove all done tasks |

## Storage

- Dimension: session
- Path: `~/.ion/agent/sessions/{hash}/data/{sid}/todo_extension/tasks`

## Events

当前版本不发射自定义事件。状态通过 `todo_list` Pull；如果未来提供多终端实时同步，必须同时增加状态变更 Push 和关闭/更新事件。

## Testing

Native unit tests (pure-logic helpers, always available):

```bash
cd extensions/todo-extension
cargo test
```

Host-side integration tests (loads the compiled WASM with wasmtime and exercises
the tools end-to-end). Requires the WASM artifact to be present:

```bash
cd extensions/todo-extension
cargo build --target wasm32-wasip1 --release

cd ../tests-extensions
cargo test --test todo_host
```

If the WASM artifact is not built, the host integration tests are skipped
gracefully (exit 0) rather than failing.

## Manual RPC smoke test

```bash
ion rpc --method create_session --params '{"agent":"developer"}'
# -> sess_xxx

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_add","args":{"text":"example task"}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"all"}}'
```
