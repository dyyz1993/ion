# todo-extension Manual

> Type: WASM extension (`todo-extension/src/lib.rs`)
> Build: `cargo build --target wasm32-wasip1 --release -p todo-extension`
> Install: `cp target/wasm32-wasip1/release/todo_extension.wasm <project>/.ion/extensions/`

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
- Path: `~/.ion/agent/sessions/{hash}/data/{sid}/todo-extension/tasks`

## Events

| customType | Description |
|-----------|-------------|
| `todo_added` | A task was created |
| `todo_done` | A task was marked done |
| `todo_removed` | A task was removed |

## Testing

Native unit tests (pure-logic helpers, always available):

```bash
cargo test -p todo-extension
```

Host-side integration tests (loads the compiled WASM with wasmtime and exercises
the tools end-to-end). Requires the WASM artifact to be present:

```bash
cargo build --target wasm32-wasip1 --release -p todo-extension
cargo test -p tests-extensions --test todo_host
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
