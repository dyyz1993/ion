# rules-engine (WASM)

Portable WASM extension for project rules injection.

## What it does

Scans `.ion/rules/*.md` files, parses YAML frontmatter for `applyTo` glob patterns,
and injects matched rules into the system prompt.

## Install

```bash
cp rules_engine.wasm ~/.ion/agent/extensions/
```

## Rule file format

Create `.ion/rules/rust.md`:
```
---
applyTo: "**/*.rs"
---

- Use snake_case for all function names
- Add doc comments for public functions
- Run cargo fmt before committing
```

## Build from source

```bash
cargo build --target wasm32-wasip1 --release
```

## Host functions used

- `host_read_file` — read rule .md files
- `host_glob` — scan .ion/rules/*.md
- `host_path_exists` — check if rules directory exists

## Hooks

- `on_system_prompt` — inject matched rules
- `on_rpc` — list/match rules via extension_rpc
