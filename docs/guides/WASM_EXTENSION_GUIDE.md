# WASM Extension Development Guide

This guide explains how to write, build, and ship a WebAssembly (WASM)
extension for the Ion runtime. WASM extensions are sandboxed plugins that
can hook into the agent lifecycle, expose custom tools, and read data from
the host without leaking unsafe code into the core process.

---

## Table of Contents

1. [Overview](#overview)
2. [Quick Start](#quick-start)
3. [Host Functions](#host-functions)
4. [Hooks](#hooks)
5. [Tutorial: Build a Linter Extension](#tutorial-build-a-linter-extension)
6. [Building and Packaging](#building-and-packaging)
7. [Configuration](#configuration)
8. [Testing](#testing)
9. [Troubleshooting](#troubleshooting)
10. [Reference](#reference)

---

## Overview

A WASM extension is a single `.wasm` module compiled from Rust (or any
language that targets `wasm32-unknown-unknown`). The module exports one
or more of the following:

- A `manifest()` function describing the extension.
- One or more hook entry points (for example `on_session_start`).
- Optional tool handlers referenced by the manifest.

The host loads the module inside a WASM runtime, validates its manifest,
and wires up the declared hooks. Extensions run in a linear memory sandbox
and communicate with the host only through the exported host functions.

### Why WASM?

- **Sandboxing** - Modules cannot touch the filesystem or network directly.
- **Portability** - A single `.wasm` artifact runs on macOS, Linux, and Windows.
- **Version safety** - The host API is versioned and explicitly declared.
- **Fast startup** - Compiled modules cold start in milliseconds.

### Supported Targets

| Host OS    | Runtime       | Notes                                   |
|------------|---------------|-----------------------------------------|
| macOS      | wasmtime      | Primary development target.             |
| Linux      | wasmtime      | Used in CI and containers.              |
| Windows    | wasmtime      | Experimental; same module format.       |

---

## Quick Start

This section creates a minimal extension that prints a greeting when a
session starts. It takes about five minutes.

### 1. Install the Rust Toolchain

You need the `wasm32-unknown-unknown` target. Install it once:

```bash
rustup target add wasm32-unknown-unknown
```

Verify:

```bash
rustc --version
cargo --version
```

### 2. Create the Crate

```bash
cargo new --lib hello-ext
cd hello-ext
```

Edit `Cargo.toml`:

```toml
[package]
name = "hello-ext"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

### 3. Write the Extension

Replace `src/lib.rs` with:

```rust
// Minimal WASM extension that greets on session start.

use serde_json::json;

// The host calls this to discover what the extension provides.
#[no_mangle]
pub extern "C" fn manifest() -> u64 {
    let manifest = json!({
        "name": "hello-ext",
        "version": "0.1.0",
        "hooks": ["on_session_start"]
    });
    let serialized = serde_json::to_vec(&manifest).unwrap();
    let len = serialized.len();
    let ptr = serialized.as_ptr() as usize;
    // Pack pointer and length into a single u64.
    // High 32 bits: pointer. Low 32 bits: length.
    std::mem::forget(serialized);
    ((ptr as u64) << 32) | (len as u64)
}

// The host invokes this when a session starts.
#[no_mangle]
pub extern "C" fn on_session_start() {
    // In a real extension we would call a host function to log.
}
```

### 4. Build the Module

```bash
cargo build --release --target wasm32-unknown-unknown
```

The artifact lands at:

```
target/wasm32-unknown-unknown/release/hello_ext.wasm
```

### 5. Register the Extension

Copy the `.wasm` file into the extensions directory and add it to the
runtime configuration (see [Configuration](#configuration)):

```bash
cp target/wasm32-unknown-unknown/release/hello_ext.wasm \
   ~/.config/ion/extensions/hello_ext.wasm
```

Start the runtime and start a session. The extension is now active.

---

## Host Functions

Host functions are the only way for an extension to interact with the
outside world. They are imported by the WASM module and implemented by
the runtime. Each host function follows the same calling convention:
the extension passes a pointer plus a length, and the host returns a
packed `(pointer, length)` descriptor.

### Available Host Functions

| Name                  | Purpose                                         |
|-----------------------|-------------------------------------------------|
| `host_log`            | Append a line to the extension log.             |
| `host_read_file`      | Read a file inside the extension sandbox root.  |
| `host_write_file`     | Write a file inside the extension sandbox root. |
| `host_http_get`       | Perform an HTTP GET request.                    |
| `host_http_post`      | Perform an HTTP POST request.                   |
| `host_get_env`        | Read an environment variable.                   |
| `host_emit_event`     | Emit a structured event on the event bus.       |
| `host_session_meta`   | Fetch metadata about the current session.       |

### Calling Convention

All buffer-returning host functions use the following pattern:

```text
u64 host_fn(ptr_in: *const u8, len_in: usize)
    -> packed (ptr_out: *const u8, len_out: usize)
```

The returned `u64` packs the pointer into the high 32 bits and the
length into the low 32 bits. The extension is responsible for freeing
the buffer through `host_free`.

### Example: Logging

```rust
extern "C" {
    fn host_log(ptr: *const u8, len: usize);
}

fn log(message: &str) {
    unsafe { host_log(message.as_ptr(), message.len()); }
}
```

### Example: Reading the Session Metadata

```rust
extern "C" {
    fn host_session_meta() -> u64;
    fn host_free(ptr: *mut u8, len: usize);
}

fn session_meta() -> serde_json::Value {
    let packed = unsafe { host_session_meta() };
    let ptr = (packed >> 32) as *mut u8;
    let len = (packed & 0xFFFF_FFFF) as usize;
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
    unsafe { host_free(ptr, len); }
    serde_json::from_slice(&bytes).unwrap_or_default()
}
```

### Memory Ownership

- Memory allocated by the host and returned to the extension **must** be
  freed by calling `host_free`.
- Memory allocated by the extension and passed to the host is **read-only**
  from the host perspective; the extension retains ownership.
- Never keep a pointer across a host call boundary that may trigger
  reallocation. Copy the data into an owned buffer first.

---

## Hooks

Hooks let an extension react to lifecycle events. Each hook is a plain
`#[no_mangle] extern "C"` function with a fixed signature. The host
discovers hooks through the manifest and calls them at the appropriate
time.

### Available Hooks

| Hook                   | Trigger                                      | Signature                          |
|------------------------|----------------------------------------------|------------------------------------|
| `on_session_start`     | A new session begins.                        | `extern "C" fn() -> u64`           |
| `on_session_end`       | A session ends.                              | `extern "C" fn() -> u64`           |
| `on_message`           | A user or assistant message is appended.     | `extern "C" fn(ptr, len) -> u64`   |
| `on_tool_call`         | A tool is about to be invoked.               | `extern "C" fn(ptr, len) -> u64`   |
| `on_tool_result`       | A tool returns its result.                   | `extern "C" fn(ptr, len) -> u64`   |
| `on_compaction`        | Context compaction runs.                     | `extern "C" fn(ptr, len) -> u64`   |

### Return Values

Hooks that return a `u64` pack a JSON response. The runtime inspects the
response for optional fields:

```json
{
  "allow": true,
  "message": "optional human-readable note"
}
```

- If `allow` is `false`, the runtime aborts the current operation and
  surfaces the `message` to the user.
- If `allow` is omitted or `true`, the operation continues normally.

### Declaring Hooks in the Manifest

The manifest lists the hooks the module exports:

```json
{
  "name": "my-ext",
  "version": "0.1.0",
  "hooks": ["on_session_start", "on_message"]
}
```

Only functions listed in the manifest are wired up. Declaring a hook
without exporting the matching function is a load-time error.

### Example: Filtering Messages

```rust
use serde_json::json;

#[no_mangle]
pub extern "C" fn on_message(ptr: *const u8, len: usize) -> u64 {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let msg: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return pack_ok(),
    };

    let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");

    // Reject messages that contain forbidden keywords.
    if content.contains("forbidden-word") {
        return pack_json(json!({
            "allow": false,
            "message": "Message blocked by extension policy."
        }));
    }

    pack_ok()
}

fn pack_ok() -> u64 {
    pack_json(json!({ "allow": true }))
}

fn pack_json(value: serde_json::Json) -> u64 {
    let bytes = serde_json::to_vec(&value).unwrap();
    let len = bytes.len();
    let ptr = bytes.as_ptr() as usize;
    std::mem::forget(bytes);
    ((ptr as u64) << 32) | (len as u64)
}
```

---

## Tutorial: Build a Linter Extension

This tutorial builds a complete extension that lints assistant messages
for common mistakes before they are shown to the user.

### Step 1: Scaffold the Crate

```bash
cargo new --lib message-linter
cd message-linter
```

`Cargo.toml`:

```toml
[package]
name = "message-linter"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

### Step 2: Define the Lint Rules

Create `src/lib.rs`:

```rust
// Message linter WASM extension.

use serde_json::{json, Value};

// List of banned phrases. In a real extension this would be loaded
// from a configuration file via host_read_file.
const BANNED: &[&str] = &[
    "I cannot help with that",
    "As an AI language model",
    "It is not within my programming",
];

#[no_mangle]
pub extern "C" fn manifest() -> u64 {
    let manifest = json!({
        "name": "message-linter",
        "version": "0.1.0",
        "hooks": ["on_message"]
    });
    pack_json(manifest)
}

#[no_mangle]
pub extern "C" fn on_message(ptr: *const u8, len: usize) -> u64 {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let msg: Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return pack_ok(),
    };

    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");

    for phrase in BANNED {
        if content.contains(phrase) {
            return pack_json(json!({
                "allow": false,
                "message": format!("Linter: banned phrase '{}'.", phrase)
            }));
        }
    }

    pack_ok()
}

fn pack_ok() -> u64 {
    pack_json(json!({ "allow": true }))
}

fn pack_json(value: Value) -> u64 {
    let bytes = serde_json::to_vec(&value).unwrap();
    let len = bytes.len();
    let ptr = bytes.as_ptr() as usize;
    std::mem::forget(bytes);
    ((ptr as u64) << 32) | (len as u64)
}
```

### Step 3: Build

```bash
cargo build --release --target wasm32-unknown-unknown
```

### Step 4: Register and Test

Copy the artifact and add the extension to the configuration:

```bash
cp target/wasm32-unknown-unknown/release/message_linter.wasm \
   ~/.config/ion/extensions/message_linter.wasm
```

Start a session and send a message containing one of the banned phrases.
The runtime blocks the message and displays the linter reason.

---

## Building and Packaging

### Directory Layout

A packaged extension is a single `.wasm` file. The recommended project
layout is:

```text
my-extension/
  Cargo.toml
  src/
    lib.rs
  README.md
```

### Build Command

Always build with the release profile and the WASM target:

```bash
cargo build --release --target wasm32-unknown-unknown
```

### Optimizing the Module

Add the following to `Cargo.toml` to reduce binary size:

```toml
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

Rebuild and check the size:

```bash
ls -lh target/wasm32-unknown-unknown/release/my_extension.wasm
```

### Versioning

Bump the version in the manifest on every change. The runtime logs the
loaded version on startup, which makes it easy to debug mismatches.

---

## Configuration

Extensions are registered in the runtime configuration file. The default
location is `~/.config/ion/config.toml`.

```toml
[[extensions]]
name = "hello-ext"
path = "~/.config/ion/extensions/hello_ext.wasm"
enabled = true

[[extensions]]
name = "message-linter"
path = "~/.config/ion/extensions/message_linter.wasm"
enabled = true
```

Each entry supports the following fields:

| Field     | Type    | Required | Description                              |
|-----------|---------|----------|------------------------------------------|
| `name`    | string  | yes      | Unique identifier for the extension.     |
| `path`    | string  | yes      | Absolute path to the `.wasm` file.       |
| `enabled` | boolean | no       | Defaults to `true`. Set to `false` to disable. |

### Environment Variables

The runtime resolves `~` and environment variables in the `path` field.
For example:

```toml
path = "${ION_EXTENSIONS_DIR}/message_linter.wasm"
```

---

## Testing

### Unit Testing

Write standard Rust unit tests inside the crate. Because the core logic
does not depend on host functions, you can test it directly:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_clean_message() {
        let response = check_message("Hello, world!");
        assert!(response.get("allow").unwrap().as_bool().unwrap());
    }

    #[test]
    fn blocks_banned_phrase() {
        let response = check_message("As an AI language model...");
        assert!(!response.get("allow").unwrap().as_bool().unwrap());
    }

    fn check_message(content: &str) -> Value {
        let msg = json!({ "content": content });
        let bytes = serde_json::to_vec(&msg).unwrap();
        let packed = on_message(bytes.as_ptr(), bytes.len());
        let ptr = (packed >> 32) as *mut u8;
        let len = (packed & 0xFFFF_FFFF) as usize;
        let data = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        serde_json::from_slice(&data).unwrap()
    }
}
```

Run the tests with:

```bash
cargo test
```

### Integration Testing

For end-to-end tests, run the runtime with the extension enabled and
verify behavior through the CLI. The test scripts in the `tests/`
directory provide patterns for scripting these checks.

---

## Troubleshooting

### Module Fails to Load

- Verify the target: `file my_extension.wasm` should report
  `WebAssembly (wasm) binary module`.
- Check that the manifest function is exported and returns valid JSON.
- Ensure the `hooks` listed in the manifest are all exported.

### Host Function Not Found

- Confirm the `extern "C"` block declares every host function you call.
- Check the spelling: host function names are case-sensitive.

### Memory Errors

- Always free host-allocated buffers with `host_free`.
- Do not retain pointers across host calls that may reallocate.
- Use `cargo build` warnings to catch unused imports early.

### Hook Not Firing

- Confirm the hook name in the manifest matches the exported function.
- Verify the extension is enabled in the configuration.
- Check the runtime logs for load-time errors.

---

## Reference

### Manifest Schema

```json
{
  "name": "string",
  "version": "semver string",
  "hooks": ["on_session_start", "on_message"],
  "tools": [
    {
      "name": "string",
      "description": "string",
      "handler": "function_name"
    }
  ]
}
```

### Hook Payload Schemas

#### `on_session_start`

No payload. The hook receives no arguments.

#### `on_session_end`

No payload.

#### `on_message`

```json
{
  "role": "user | assistant | system",
  "content": "string"
}
```

#### `on_tool_call`

```json
{
  "tool": "string",
  "input": { ... }
}
```

#### `on_tool_result`

```json
{
  "tool": "string",
  "output": "string"
}
```

#### `on_compaction`

```json
{
  "reason": "string",
  "entries_removed": 42
}
```

### Full Host Function Signature List

```text
host_log(ptr: *const u8, len: usize)
host_read_file(ptr: *const u8, len: usize) -> u64
host_write_file(ptr: *const u8, len_in: usize, ptr2: *const u8, len2: usize) -> u64
host_http_get(ptr: *const u8, len: usize) -> u64
host_http_post(ptr: *const u8, len: usize, ptr2: *const u8, len2: usize) -> u64
host_get_env(ptr: *const u8, len: usize) -> u64
host_emit_event(ptr: *const u8, len: usize) -> u64
host_session_meta() -> u64
host_free(ptr: *mut u8, len: usize)
```

### Glossary

- **Host** - The Ion runtime process that loads and runs WASM modules.
- **Module** - A compiled `.wasm` artifact.
- **Manifest** - JSON document describing what an extension provides.
- **Hook** - A lifecycle event that an extension can subscribe to.
- **Host Function** - A function implemented by the host and imported by the module.
- **Sandbox** - The isolated linear memory and capability set granted to a module.

---

_End of guide. For questions, open an issue in the main repository._
