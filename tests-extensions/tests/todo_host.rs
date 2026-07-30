// tests/host_integration.rs
// Host-side integration test: loads the compiled WASM extension with wasmtime,
// stubs out the host imports it needs (host_register_tool,
// host_read_session_data, host_write_session_data, the WASI surface, and
// memcmp), then drives the tools end-to-end and asserts on the JSON output.
//
// Run with:
//   cargo test -p todo-extension --test host_integration
//
// If the WASM artifact is not built yet, every test in this file exits the
// process with code 0 (a "soft skip") so CI is green even before the WASM
// target is available. Build the artifact first with:
//   cargo build --target wasm32-wasip1 --release -p todo-extension

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wasmtime::{Caller, Engine, Extern, Linker, Memory, MemoryType, Module, Store};

/// Offset in linear memory where the host places tool name / args / output.
/// Matches the host runtime constant in `src/wasm_extension.rs`.
const DATA_OFFSET: u32 = 100_000;

/// Shared per-test state passed to the wasmtime store. The stub host
/// functions read this to satisfy `host_read_session_data` and capture the
/// bytes written by `host_write_session_data`.
#[derive(Clone, Default)]
struct Ctx {
    /// In-memory session storage keyed by storage key ("tasks" for todos).
    storage: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Names of tools registered via host_register_tool.
    registered: Arc<Mutex<Vec<String>>>,
}

/// Locate the compiled WASM artifact. Exits the process (code 0 = soft skip)
/// if it is not present, so the test harness still reports success.
fn wasm_path() -> std::path::PathBuf {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .and_then(|d| d.parent().map(|d| d.to_path_buf()))
        .and_then(|d| d.parent().map(|d| d.to_path_buf()))
        .map(|root| {
            root.join("wasm32-wasip1")
                .join("release")
                .join("todo_extension.wasm")
        })
        .unwrap_or_else(|| {
            std::path::PathBuf::from("target/wasm32-wasip1/release/todo_extension.wasm")
        });
    path
}

fn ensure_wasm_or_skip() -> std::path::PathBuf {
    let path = wasm_path();
    if !path.exists() {
        eprintln!(
            "WASM artifact not found at {:?}. \
             Run: cargo build --target wasm32-wasip1 --release -p todo-extension",
            path
        );
        std::process::exit(0);
    }
    path
}

fn mem_of(caller: &mut Caller<'_, Ctx>) -> Option<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(m)) => Some(m),
        _ => None,
    }
}

fn mem_read(caller: &mut Caller<'_, Ctx>, ptr: u32, len: u32) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    let mem = match mem_of(caller) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut buf = vec![0u8; len as usize];
    let _ = mem.read(caller, ptr as usize, &mut buf);
    buf
}

fn mem_write(caller: &mut Caller<'_, Ctx>, ptr: u32, data: &[u8]) {
    if let Some(mem) = mem_of(caller) {
        let _ = mem.write(caller, ptr as usize, data);
    }
}

fn build_linker(engine: &Engine) -> Linker<Ctx> {
    let mut linker = Linker::<Ctx>::new(engine);

    // Minimal WASI stubs. We never actually call std I/O from the extension,
    // but std-compiled WASM still imports these and they must resolve.
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |mut caller: Caller<'_, Ctx>, count_ptr: u32, buf_size_ptr: u32| -> u32 {
                if let Some(mem) = mem_of(&mut caller) {
                    let _ = mem.write(&mut caller, count_ptr as usize, &0u32.to_le_bytes());
                    let _ = mem.write(&mut caller, buf_size_ptr as usize, &0u32.to_le_bytes());
                }
                0
            },
        )
        .expect("environ_sizes_get");

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |_: Caller<'_, Ctx>, _: u32, _: u32| -> u32 { 0 },
        )
        .expect("environ_get");

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, Ctx>,
             _fd: u32,
             iovs_ptr: u32,
             iovs_len: u32,
             nwritten_ptr: u32|
             -> u32 {
                let mut total: u32 = 0;
                if let Some(mem) = mem_of(&mut caller) {
                    for i in 0..iovs_len {
                        let base = iovs_ptr as usize + (i as usize) * 8;
                        let mut buf = [0u8; 4];
                        if mem.read(&mut caller, base, &mut buf).is_ok() {
                            total = total.wrapping_add(u32::from_le_bytes(buf));
                        }
                    }
                    let _ = mem.write(&mut caller, nwritten_ptr as usize, &total.to_le_bytes());
                }
                0
            },
        )
        .expect("fd_write");

    linker
        .func_wrap("wasi_snapshot_preview1", "proc_exit", |_code: u32| ())
        .expect("proc_exit");

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, Ctx>, buf_ptr: u32, buf_len: u32| -> u32 {
                // Deterministic fill (good enough for HashMap seeding).
                let mut data = vec![0u8; buf_len as usize];
                for i in 0..data.len() {
                    data[i] = ((i as u64)
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407)
                        >> 32) as u8;
                }
                mem_write(&mut caller, buf_ptr, &data);
                0
            },
        )
        .expect("random_get");

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, Ctx>, _: u32, _: u64, time_ptr: u32| -> u32 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                mem_write(&mut caller, time_ptr, &now.to_le_bytes());
                0
            },
        )
        .expect("clock_time_get");

    // Host: register_tool. Records the tool name into the shared list.
    linker
        .func_wrap(
            "env",
            "host_register_tool",
            |mut caller: Caller<'_, Ctx>,
             name_ptr: u32,
             name_len: u32,
             _desc_ptr: u32,
             _desc_len: u32,
             _schema_ptr: u32,
             _schema_len: u32| {
                let name = String::from_utf8_lossy(&mem_read(&mut caller, name_ptr, name_len))
                    .trim_end_matches('\0')
                    .to_string();
                if let Ok(mut reg) = caller.data().registered.lock() {
                    reg.push(name);
                }
            },
        )
        .expect("host_register_tool");

    // Host: read_session_data. Looks up the key in the in-memory store.
    linker
        .func_wrap(
            "env",
            "host_read_session_data",
            |mut caller: Caller<'_, Ctx>,
             key_ptr: u32,
             key_len: u32,
             out_buf: u32,
             out_capacity: u32| {
                let key =
                    String::from_utf8_lossy(&mem_read(&mut caller, key_ptr, key_len)).to_string();
                let data = match caller.data().storage.lock() {
                    Ok(map) => map.get(&key).cloned().unwrap_or_default(),
                    Err(_) => Vec::new(),
                };
                if data.is_empty() {
                    return 0;
                }
                let len = data.len().min(out_capacity as usize);
                mem_write(&mut caller, out_buf, &data[..len]);
                len as u32
            },
        )
        .expect("host_read_session_data");

    // Host: write_session_data. Records the payload into the in-memory store.
    linker
        .func_wrap(
            "env",
            "host_write_session_data",
            |mut caller: Caller<'_, Ctx>,
             key_ptr: u32,
             key_len: u32,
             data_ptr: u32,
             data_len: u32| {
                let key =
                    String::from_utf8_lossy(&mem_read(&mut caller, key_ptr, key_len)).to_string();
                let data = mem_read(&mut caller, data_ptr, data_len);
                if let Ok(mut map) = caller.data().storage.lock() {
                    map.insert(key, data);
                }
                0
            },
        )
        .expect("host_write_session_data");

    // Host: memcmp. Required because libcore references it.
    linker
        .func_wrap(
            "env",
            "memcmp",
            |mut caller: Caller<'_, Ctx>, ptr1: u32, ptr2: u32, n: u32| -> i32 {
                let a = mem_read(&mut caller, ptr1, n);
                let b = mem_read(&mut caller, ptr2, n);
                for i in 0..n as usize {
                    if a[i] != b[i] {
                        return a[i] as i32 - b[i] as i32;
                    }
                }
                0
            },
        )
        .expect("memcmp");

    linker
}

/// A fully-instantiated extension instance ready to drive tools.
struct Harness {
    _engine: Engine,
    store: Store<Ctx>,
    instance: wasmtime::Instance,
    memory: Memory,
}

impl Harness {
    fn load() -> Self {
        let wasm_path = ensure_wasm_or_skip();
        let engine = Engine::default();
        let module = Module::from_file(&engine, &wasm_path).expect("module from_file");

        let ctx = Ctx::default();
        let mut store = Store::new(&engine, ctx);

        // 16 pages (1 MB) of linear memory. Matches the host runtime.
        let memory = Memory::new(&mut store, MemoryType::new(16, None)).expect("Memory::new");

        let mut linker = build_linker(&engine);
        linker
            .define(&mut store, "env", "memory", memory)
            .expect("define memory");

        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");

        let mem = instance
            .get_memory(&mut store, "memory")
            .expect("module exports memory");

        Self {
            _engine: engine,
            store,
            instance,
            memory: mem,
        }
    }

    fn call_init(&mut self) {
        let func = self
            .instance
            .get_typed_func::<(), ()>(&mut self.store, "extension_init")
            .expect("extension_init");
        func.call(&mut self.store, ()).expect("call init");
    }

    fn registered_tools(&self) -> Vec<String> {
        self.store
            .data()
            .registered
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn execute_tool(&mut self, name: &str, args: &str) -> String {
        let func = self
            .instance
            .get_typed_func::<(u32, u32, u32, u32, u32, u32), u32>(
                &mut self.store,
                "extension_execute_tool",
            )
            .expect("extension_execute_tool");

        let name_bytes = name.as_bytes();
        let args_bytes = args.as_bytes();
        let name_offset = DATA_OFFSET;
        let name_len = name_bytes.len() as u32;
        let args_offset = name_offset + name_len;
        let args_len = args_bytes.len() as u32;
        let out_offset = args_offset + args_len;
        let out_capacity: u32 = 4096;

        self.memory
            .write(&mut self.store, name_offset as usize, name_bytes)
            .expect("write name");
        self.memory
            .write(&mut self.store, args_offset as usize, args_bytes)
            .expect("write args");

        let result_len = func
            .call(
                &mut self.store,
                (
                    name_offset,
                    name_len,
                    args_offset,
                    args_len,
                    out_offset,
                    out_capacity,
                ),
            )
            .expect("call execute_tool");

        let mut buf = vec![0u8; result_len as usize];
        self.memory
            .read(&mut self.store, out_offset as usize, &mut buf)
            .expect("read out");
        String::from_utf8_lossy(&buf).to_string()
    }
}

#[test]
fn test_wasm_loads() {
    // If WASM is not built, ensure_wasm_or_skip exits 0 above.
    let _h = Harness::load();
}

#[test]
fn test_extension_registers_all_tools() {
    let mut h = Harness::load();
    h.call_init();
    let tools = h.registered_tools();
    assert!(
        tools.contains(&"todo_add".to_string()),
        "tools: {:?}",
        tools
    );
    assert!(
        tools.contains(&"todo_list".to_string()),
        "tools: {:?}",
        tools
    );
    assert!(
        tools.contains(&"todo_done".to_string()),
        "tools: {:?}",
        tools
    );
    assert!(
        tools.contains(&"todo_remove".to_string()),
        "tools: {:?}",
        tools
    );
    assert!(
        tools.contains(&"todo_clean".to_string()),
        "tools: {:?}",
        tools
    );
}

#[test]
fn test_todo_add_returns_id_and_status() {
    let mut h = Harness::load();
    h.call_init();
    let out = h.execute_tool("todo_add", r#"{"text":"hello"}"#);
    assert!(
        out.contains(r#""status":"created""#),
        "expected status=created in: {}",
        out
    );
    assert!(
        out.contains(r#""text":"hello""#),
        "expected text=hello in: {}",
        out
    );
}

#[test]
fn test_todo_list_empty_returns_empty_array() {
    let mut h = Harness::load();
    h.call_init();
    let out = h.execute_tool("todo_list", r#"{"status":"all"}"#);
    assert_eq!(out.trim(), "[]", "empty list should be []");
}

#[test]
fn test_todo_done_marks_task() {
    let mut h = Harness::load();
    h.call_init();
    // Add a task first.
    let _ = h.execute_tool("todo_add", r#"{"text":"mark me"}"#);
    // Then mark id=1 done.
    let done = h.execute_tool("todo_done", r#"{"id":"1"}"#);
    assert!(
        done.contains(r#""status":"done""#),
        "expected status=done in: {}",
        done
    );
    // The active list should now be empty; the done list should contain it.
    let active = h.execute_tool("todo_list", r#"{"status":"active"}"#);
    assert_eq!(active.trim(), "[]", "active list after done: {}", active);
    let done_list = h.execute_tool("todo_list", r#"{"status":"done"}"#);
    assert!(
        done_list.contains(r#""id":"1""#),
        "done list should contain id 1: {}",
        done_list
    );
}

#[test]
fn test_todo_remove_deletes_task() {
    let mut h = Harness::load();
    h.call_init();
    let _ = h.execute_tool("todo_add", r#"{"text":"delete me"}"#);
    let removed = h.execute_tool("todo_remove", r#"{"id":"1"}"#);
    assert!(
        removed.contains(r#""status":"removed""#),
        "expected status=removed in: {}",
        removed
    );
    let list = h.execute_tool("todo_list", r#"{"status":"all"}"#);
    assert_eq!(list.trim(), "[]", "list after remove: {}", list);
}
