# rules-engine 手册

> **状态：已验证** — 预构建 WASM 的规则扫描、匹配和 system prompt 注入已验证；本目录当前不包含源码。
>
> **类型：** 运行时 WASM Extension
> **Extension ID：** `rules_engine`
> **ABI 版本：** `1`

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

本目录当前只分发预构建 `rules_engine.wasm`。以下命令仅在取得对应源码后适用：

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
