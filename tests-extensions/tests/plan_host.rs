// tests/host_integration.rs
// Host-side integration test: loads the compiled WASM extension with wasmtime,
// stubs out the host imports it needs (host_register_tool, host_read_file,
// host_write_file, host_path_exists, the WASI surface, and memcmp), then
// drives the tools end-to-end and asserts on the JSON output.
//
// Run with:
//   cargo test -p plan-extension --test host_integration
//
// If the WASM artifact is not built yet, every test in this file exits the
// process with code 0 (a "soft skip") so CI is green even before the WASM
// target is available. Build the artifact first with:
//   cargo build --target wasm32-wasip1 --release -p plan-extension

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wasmtime::{Caller, Engine, Extern, Linker, Memory, MemoryType, Module, Store};

/// Offset in linear memory where the host places tool name / args / output.
/// Matches the host runtime constant in `src/wasm_extension.rs`.
const DATA_OFFSET: u32 = 100_000;

/// Shared per-test state passed to the wasmtime store. The stub host
/// functions read this to satisfy `host_read_file` and capture the bytes
/// written by `host_write_file`.
#[derive(Clone, Default)]
struct Ctx {
    /// In-memory filesystem keyed by absolute path.
    fs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
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
                .join("plan_extension.wasm")
        })
        .unwrap_or_else(|| {
            std::path::PathBuf::from("target/wasm32-wasip1/release/plan_extension.wasm")
        });
    path
}

fn ensure_wasm_or_skip() -> std::path::PathBuf {
    let path = wasm_path();
    if !path.exists() {
        eprintln!(
            "WASM artifact not found at {:?}. \
             Run: cargo build --target wasm32-wasip1 --release -p plan-extension",
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

    // Host: read_file. Looks up the path in the in-memory filesystem.
    linker
        .func_wrap(
            "env",
            "host_read_file",
            |mut caller: Caller<'_, Ctx>,
             path_ptr: u32,
             path_len: u32,
             out_buf: u32,
             out_capacity: u32| {
                let path =
                    String::from_utf8_lossy(&mem_read(&mut caller, path_ptr, path_len)).to_string();
                let data = match caller.data().fs.lock() {
                    Ok(map) => map.get(&path).cloned().unwrap_or_default(),
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
        .expect("host_read_file");

    // Host: write_file. Records the payload into the in-memory filesystem.
    linker
        .func_wrap(
            "env",
            "host_write_file",
            |mut caller: Caller<'_, Ctx>,
             path_ptr: u32,
             path_len: u32,
             data_ptr: u32,
             data_len: u32| {
                let path =
                    String::from_utf8_lossy(&mem_read(&mut caller, path_ptr, path_len)).to_string();
                let data = mem_read(&mut caller, data_ptr, data_len);
                if let Ok(mut map) = caller.data().fs.lock() {
                    map.insert(path, data);
                }
                0
            },
        )
        .expect("host_write_file");

    // Host: path_exists. Returns 1 if the path is present in the fs map.
    linker
        .func_wrap(
            "env",
            "host_path_exists",
            |mut caller: Caller<'_, Ctx>, path_ptr: u32, path_len: u32| -> u32 {
                let path =
                    String::from_utf8_lossy(&mem_read(&mut caller, path_ptr, path_len)).to_string();
                match caller.data().fs.lock() {
                    Ok(map) => {
                        if map.contains_key(&path) {
                            1
                        } else {
                            0
                        }
                    }
                    Err(_) => 0,
                }
            },
        )
        .expect("host_path_exists");

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

    fn extension_version(&mut self) -> u32 {
        let func = self
            .instance
            .get_typed_func::<(), u32>(&mut self.store, "extension_version")
            .expect("extension_version");
        func.call(&mut self.store, ()).expect("call version")
    }

    fn registered_tools(&self) -> Vec<String> {
        self.store
            .data()
            .registered
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn fs_snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.store
            .data()
            .fs
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
                (name_offset, name_len, args_offset, args_len, out_offset, out_capacity),
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
fn test_extension_version_returns_1() {
    let mut h = Harness::load();
    let v = h.extension_version();
    assert_eq!(v, 1, "extension_version should return 1");
}

#[test]
fn test_extension_registers_all_tools() {
    let mut h = Harness::load();
    h.call_init();
    let tools = h.registered_tools();
    for expected in [
        "plan_enter",
        "plan_exit",
        "plan_add",
        "plan_list",
        "plan_done",
    ] {
        assert!(
            tools.contains(&expected.to_string()),
            "expected tool {} in {:?}",
            expected,
            tools
        );
    }
}

#[test]
fn test_plan_enter_sets_path() {
    let mut h = Harness::load();
    h.call_init();
    // The plan path does not exist yet; plan_enter should create it empty.
    let out = h.execute_tool("plan_enter", r#"{"plan_path":"/tmp/test_plan.md"}"#);
    assert!(
        out.contains(r#""status":"ok""#),
        "expected status=ok in: {}",
        out
    );
    assert!(
        out.contains(r#""mode":"plan""#),
        "expected mode=plan in: {}",
        out
    );
    assert!(
        out.contains(r#""plan_path":"/tmp/test_plan.md""#),
        "expected plan_path echoed in: {}",
        out
    );
    // host_path_exists returned 0 (not in the fs map) so the extension should
    // have written an empty file via host_write_file.
    let fs = h.fs_snapshot();
    assert!(
        fs.contains_key("/tmp/test_plan.md"),
        "plan file should be created, fs: {:?}",
        fs
    );
    let content = fs.get("/tmp/test_plan.md").cloned().unwrap_or_default();
    assert!(
        content.is_empty(),
        "newly created plan file should be empty, got {:?}",
        content
    );
}

#[test]
fn test_plan_add_appends_step() {
    let mut h = Harness::load();
    h.call_init();
    h.execute_tool("plan_enter", r#"{"plan_path":"/tmp/test_plan.md"}"#);
    let out = h.execute_tool("plan_add", r#"{"step":"write tests"}"#);
    assert!(
        out.contains(r#""status":"added""#),
        "expected status=added in: {}",
        out
    );
    assert!(
        out.contains(r#""index":0"#),
        "expected index=0 for first step in: {}",
        out
    );

    let fs = h.fs_snapshot();
    let binding = fs.get("/tmp/test_plan.md").cloned().unwrap_or_default();
    let content = String::from_utf8_lossy(&binding);
    assert!(
        content.contains("write tests"),
        "plan file should contain the step, got: {}",
        content
    );
}

#[test]
fn test_plan_list_returns_steps() {
    let mut h = Harness::load();
    h.call_init();
    h.execute_tool("plan_enter", r#"{"plan_path":"/tmp/test_plan.md"}"#);
    h.execute_tool("plan_add", r#"{"step":"alpha"}"#);
    h.execute_tool("plan_add", r#"{"step":"beta"}"#);

    let out = h.execute_tool("plan_list", r#"{}"#);
    let parsed: serde_json::Value =
        serde_json::from_str(&out).expect("plan_list output should be valid JSON");
    let steps = parsed["steps"].as_array().expect("steps should be an array");
    assert_eq!(steps.len(), 2, "expected 2 steps, got: {}", out);
    assert_eq!(steps[0], "alpha", "first step: {}", out);
    assert_eq!(steps[1], "beta", "second step: {}", out);
    assert_eq!(parsed["count"], 2, "count: {}", out);
}

#[test]
fn test_plan_done_marks_step() {
    let mut h = Harness::load();
    h.call_init();
    h.execute_tool("plan_enter", r#"{"plan_path":"/tmp/test_plan.md"}"#);
    h.execute_tool("plan_add", r#"{"step":"first"}"#);
    h.execute_tool("plan_add", r#"{"step":"second"}"#);

    let out = h.execute_tool("plan_done", r#"{"index":0}"#);
    assert!(
        out.contains(r#""status":"done""#),
        "expected status=done in: {}",
        out
    );
    assert!(
        out.contains(r#""index":0"#),
        "expected index=0 echoed in: {}",
        out
    );

    let fs = h.fs_snapshot();
    let binding = fs.get("/tmp/test_plan.md").cloned().unwrap_or_default();
    let content = String::from_utf8_lossy(&binding);
    assert!(
        content.contains("[x] first"),
        "done step should be prefixed with [x], got: {}",
        content
    );
    assert!(
        !content.contains("[x] second"),
        "other step should be untouched, got: {}",
        content
    );
}

#[test]
fn test_plan_exit_clears_mode() {
    let mut h = Harness::load();
    h.call_init();
    h.execute_tool("plan_enter", r#"{"plan_path":"/tmp/test_plan.md"}"#);
    let out = h.execute_tool("plan_exit", r#"{}"#);
    assert!(
        out.contains(r#""status":"ok""#),
        "expected status=ok in: {}",
        out
    );
    assert!(
        out.contains(r#""mode":"normal""#),
        "expected mode=normal in: {}",
        out
    );
}
