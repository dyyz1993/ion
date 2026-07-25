# plan-extension Manual

> Type: WASM extension (`plan-extension/src/lib.rs`)
> Build: `cargo build --target wasm32-wasip1 --release -p plan-extension`
> Install: `cp target/wasm32-wasip1/release/plan_extension.wasm <project>/.ion/extensions/`

## Capabilities

- Manage a single plan file on disk (enter / exit / add / list / mark done)
- Plan path is remembered while in plan mode and cleared on exit
- The plan file is created empty if it does not exist on `plan_enter`

## Tools

| Name | Params | Description |
|------|--------|-------------|
| `plan_enter` | `{plan_path}` | Enter plan mode and remember the plan file path; create the file if missing |
| `plan_exit` | `{}` | Exit plan mode and clear the remembered path |
| `plan_add` | `{step}` | Append a step line to the plan file |
| `plan_list` | `{}` | List all steps in the plan file |
| `plan_done` | `{index}` | Mark the step at the 0-based index as done (prefix `[x] `) |

## Plan File Format

A plain text file with one step per line:

- Pending steps have no prefix, e.g. `Write the README`
- Completed steps are prefixed with `[x] `, e.g. `[x] Write the README`
- Blank lines are ignored when listing

Example:

```
[x] Sketch the design
[x] Write the spec
Implement the parser
Add tests
```

## Storage

- The plan path is provided by the caller in `plan_enter({plan_path})`.
- The extension remembers the path in a static buffer while in plan mode.
- All reads/writes go through the host filesystem capability
  (`host_read_file` / `host_write_file` / `host_path_exists`).

## Testing

Native unit tests (pure-logic helpers, always available):

```bash
cargo test -p plan-extension
```

Host-side integration tests (loads the compiled WASM with wasmtime and
exercises the tools end-to-end). Requires the WASM artifact to be present:

```bash
cargo build --target wasm32-wasip1 --release -p plan-extension
cargo test -p plan-extension --test host_integration
```

If the WASM artifact is not built, the host integration tests are skipped
gracefully (exit 0) rather than failing.

## Manual RPC smoke test

```bash
ion rpc --method create_session --params '{"agent":"developer"}'
# -> sess_xxx

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"plan_enter","args":{"plan_path":"/tmp/plan.md"}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"plan_add","args":{"step":"Sketch the design"}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"plan_list","args":{}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"plan_done","args":{"index":0}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"plan_exit","args":{}}'
```
