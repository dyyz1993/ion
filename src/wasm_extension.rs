use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use wasmtime::{Engine, Linker, Memory, MemoryType, Module, Store};

use async_trait::async_trait;
use serde::Serialize;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;
use crate::paths;

/// Offset into linear memory where the host places tool name / args / output
/// for the WASM plugin. Must be high enough to avoid the WASM module's data
/// section (HEAP at low addresses) but low enough to avoid the stack
/// (which grows downward from ~1 MB).
const DATA_OFFSET: u32 = 100_000; // 100 KB — past HEAP, before stack

// ---------------------------------------------------------------------------
// Context — injected into the WASM store so host functions know where
// to read/write plugin data files.
// ---------------------------------------------------------------------------

/// Context injected into the WASM store before every tool execution.
///
/// Host functions (`host_write_global_data`, etc.) read this from the
/// `Caller` to determine file paths for the four storage dimensions.
#[derive(Clone, Debug)]
pub struct Context {
    /// Current session ID (used for session-level data paths).
    pub session_id: String,
    /// Current working directory (used for session/project paths).
    pub cwd: String,
    /// Project root directory (used for project-local paths).
    pub project_root: String,
    /// Extension name (the subdirectory name inside each data dir).
    pub ext_name: String,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cwd: String::new(),
            project_root: String::new(),
            ext_name: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

/// A loaded WASM plugin instance with registered tools.
#[allow(dead_code)]
pub struct Extension {
    engine: Engine,
    store: Store<Context>,
    instance: wasmtime::Instance,
    memory: Memory,
    /// Tools registered by the plugin during init.
    pub tools: Vec<ToolDef>,
    /// Version number returned by plugin_version().
    pub version: u32,
}

#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl Extension {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)?;
        let mut store = Store::new(&engine, Context::default());
        let mut linker = Linker::new(&engine);

        // Use 16 pages (1 MB) initial memory. The shared data area at
        // DATA_OFFSET (100 KB) is past the module's data+heap section
        // (max ~80 KB) and well within the addressable range.
        let memory = Memory::new(&mut store, MemoryType::new(16, None))?;

        let tools_registered: Arc<Mutex<Vec<ToolDef>>> =
            Arc::new(Mutex::new(Vec::new()));
        let tools = tools_registered.clone();

    // ── Host: register_tool ──────────────────────────────────────────────
    linker.func_wrap("env", "host_register_tool",
        move |mut caller: wasmtime::Caller<'_, Context>,
              name_ptr: u32, name_len: u32,
              desc_ptr: u32, desc_len: u32,
              schema_ptr: u32, schema_len: u32| {
                let name = mem_read_str(&mut caller, name_ptr, name_len);
                let desc = mem_read_str(&mut caller, desc_ptr, desc_len);
                let schema_str = mem_read_str(&mut caller, schema_ptr, schema_len);
                let params = serde_json::from_str(&schema_str).unwrap_or_default();
                if let Ok(mut t) = tools.lock() {
                    t.push(ToolDef { name, description: desc, parameters: params });
                }
            }
    )?;

    // ── Host: send_message ──────────────────────────────────────────────
    linker.func_wrap("env", "host_send_message",
        |mut caller: wasmtime::Caller<'_, Context>,
         msg_ptr: u32, msg_len: u32| {
            if msg_len == 0 { return; }
            let msg = mem_read_str(&mut caller, msg_ptr, msg_len);
            let event = serde_json::json!({
                "type": "event",
                "event": {"type": "custom", "customType": "plugin_message",
                          "data": {"text": msg}}
            });
            eprintln!("{}", serde_json::to_string(&event).unwrap_or_default());
        }
    )?;

    // ── Host: channel_send ──────────────────────────────────────────────
    linker.func_wrap("env", "host_channel_send",
        |mut caller: wasmtime::Caller<'_, Context>,
         ch_ptr: u32, ch_len: u32,
         msg_ptr: u32, msg_len: u32| {
            let channel = mem_read_str(&mut caller, ch_ptr, ch_len);
            let msg_str = mem_read_str(&mut caller, msg_ptr, msg_len);
            let cmd = serde_json::json!({
                "type": "channel_send",
                "channel": channel,
                "msg": serde_json::from_str::<serde_json::Value>(&msg_str)
                    .unwrap_or(serde_json::Value::String(msg_str))
            });
            println!("{}", serde_json::to_string(&cmd).unwrap_or_default());
        }
    )?;

    // ── Host: create_worker ─────────────────────────────────────────────
    linker.func_wrap("env", "host_create_worker",
        |mut caller: wasmtime::Caller<'_, Context>,
         cfg_ptr: u32, cfg_len: u32| -> u32 {
            if cfg_len == 0 { return 0; }
            let cfg_str = mem_read_str(&mut caller, cfg_ptr, cfg_len);
            let cmd = serde_json::json!({
                "type": "create_worker",
                "config": serde_json::from_str::<serde_json::Value>(&cfg_str)
                    .unwrap_or(serde_json::Value::Null)
            });
            println!("{}", serde_json::to_string(&cmd).unwrap_or_default());
            0
        }
    )?;

    // ── Host: memcmp ────────────────────────────────────────────────────
    linker.func_wrap("env", "memcmp",
        |mut caller: wasmtime::Caller<'_, Context>,
         ptr1: u32, ptr2: u32, n: u32| -> i32 {
                let mem = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(m)) => m,
                    _ => return 0,
                };
                let mut buf1 = vec![0u8; n as usize];
                let mut buf2 = vec![0u8; n as usize];
                if mem.read(&mut caller, ptr1 as usize, &mut buf1).is_err() { return 0; }
                if mem.read(&mut caller, ptr2 as usize, &mut buf2).is_err() { return 0; }
                for i in 0..n as usize {
                    if buf1[i] != buf2[i] {
                        return buf1[i] as i32 - buf2[i] as i32;
                    }
                }
                0
            }
    )?;

        // ── Register data dimension host functions (4 dims × 4 ops = 16) ───
        register_dim(&mut linker, "global".into(), |ctx| paths::global_data_dir(&ctx.ext_name))?;
        register_dim(&mut linker, "project".into(), |ctx| paths::project_data_dir(&ctx.project_root, &ctx.ext_name))?;
        register_dim(&mut linker, "project_local".into(), |ctx| paths::project_local_data_dir(&ctx.project_root, &ctx.ext_name))?;
        register_dim(&mut linker, "session".into(), |ctx| paths::session_data_dir(&ctx.cwd, &ctx.session_id, &ctx.ext_name))?;

        linker.define(&mut store, "env", "memory", memory)?;
        let instance = linker.instantiate(&mut store, &module)?;

        // Use the INSTANCE's memory (whether imported or self-defined).
        let instance_memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("module does not export 'memory'")?;

        let mut plugin = Self {
            engine,
            store,
            instance,
            memory: instance_memory,
            tools: Vec::new(),
            version: 0,
        };

        // Call plugin_version
        if let Ok(func) = plugin.instance.get_typed_func::<(), u32>(&mut plugin.store, "plugin_version") {
            if let Ok(ver) = func.call(&mut plugin.store, ()) {
                plugin.version = ver;
                tracing::info!("[wasm] plugin v{ver}");
            }
        }

        // Call plugin_init — this triggers host_register_tool callbacks
        if let Ok(func) = plugin.instance.get_typed_func::<(), ()>(&mut plugin.store, "plugin_init") {
            func.call(&mut plugin.store, ())?;
        }

        drop(linker);

        // Collect registered tools
        if let Ok(t) = tools_registered.lock() {
            plugin.tools = t.clone();
        }
        tracing::info!("[wasm] plugin registered {} tools", plugin.tools.len());

        Ok(plugin)
    }

    /// Inject a new context into the WASM store (called before tool execution).
    pub fn set_context(&mut self, ctx: &Context) {
        *self.store.data_mut() = ctx.clone();
    }

    pub fn execute_tool(&mut self, name: &str, args: &str) -> Result<String, Box<dyn std::error::Error>> {
        let func = self.instance.get_typed_func::<(u32, u32, u32, u32, u32, u32), u32>(
            &mut self.store, "plugin_execute_tool"
        )?;

        let name_bytes = name.as_bytes();
        let args_bytes = args.as_bytes();
        let name_offset = DATA_OFFSET;
        let name_len = name_bytes.len() as u32;
        let args_offset = name_offset + name_len;
        let args_len = args_bytes.len() as u32;
        let out_offset = args_offset + args_len;
        let out_capacity = 4096u32;

        self.memory.write(&mut self.store, name_offset as usize, name_bytes)?;
        self.memory.write(&mut self.store, args_offset as usize, args_bytes)?;

        let result_len = func.call(&mut self.store,
            (name_offset, name_len, args_offset, args_len, out_offset, out_capacity))?;

        let mut buf = vec![0u8; result_len as usize];
        self.memory.read(&mut self.store, out_offset as usize, &mut buf)?;
        Ok(String::from_utf8_lossy(&buf).to_string())
    }
}

// ---------------------------------------------------------------------------
// Context injection helpers
// ---------------------------------------------------------------------------

/// Return a `Context` with the registry's shared context + the given ext_name.
/// This is what `ToolAdapter::execute()` uses before calling the plugin.
pub fn make_exec_context(registry_ctx: &Context, ext_name: &str) -> Context {
    let mut ctx = registry_ctx.clone();
    ctx.ext_name = ext_name.to_string();
    ctx
}

/// Derive a friendly extension name from the canonical WASM path.
/// Uses the file stem (filename without extension).
/// Examples:
/// - `todo_plugin.wasm` → `todo_plugin`
/// - `stock_plugin.wasm` → `stock_plugin`
pub fn ext_name_from_path(canonical_path: &str) -> String {
    Path::new(canonical_path)
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin".into())
}

// ---------------------------------------------------------------------------
// Registry — central registry for hot-pluggable WASM plugin instances
// ---------------------------------------------------------------------------

/// Information about a loaded plugin, returned by [`Registry::list()`].
#[derive(Clone, Debug, Serialize)]
pub struct Info {
    pub path: String,
    pub version: u32,
    pub tools: Vec<String>,
}

struct Entry {
    plugin: Arc<Mutex<Extension>>,
    version: u32,
    tool_names: Vec<String>,
    canonical_path: String,
    ext_name: String,
}

/// Central registry for WASM plugin instances with hot‑reload support.
///
/// Each loaded plugin is wrapped in `Arc<Mutex<Extension>>`. `ToolAdapter`
/// holds a reference to the registry + canonical path, so add/remove/reload
/// swap out the underlying instance without invalidating tool references.
pub struct Registry {
    plugins: RwLock<HashMap<String, Entry>>,
    /// Shared session-level context (cwd, session_id, project_root).
    /// Updated from outside (CLI / Worker) before each prompt execution.
    pub ctx: RwLock<Context>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            plugins: RwLock::new(HashMap::new()),
            ctx: RwLock::new(Context::default()),
        }
    }

    /// Add (or reload) a plugin from the given path.
    ///
    /// If the path was already registered, the old instance is replaced.
    /// Returns the tool definitions that should be registered in `ToolRegistry`.
    pub fn add(&self, path: &str) -> Result<Vec<ToolDef>, Box<dyn std::error::Error>> {
        let canonical = std::fs::canonicalize(path)?;
        let canonical_str = canonical.to_string_lossy().to_string();

        let plugin = Extension::load(&canonical)?;
        let version = plugin.version;
        let tool_defs = plugin.tools.clone();
        let tool_names: Vec<String> = tool_defs.iter().map(|t| t.name.clone()).collect();
        let ext_name = ext_name_from_path(&canonical_str);

        let entry = Entry {
            plugin: Arc::new(Mutex::new(plugin)),
            version,
            tool_names,
            canonical_path: canonical_str.clone(),
            ext_name,
        };

        self.plugins.write().unwrap().insert(canonical_str, entry);
        Ok(tool_defs)
    }

    /// Remove a plugin by path. Returns the names of tools that were unregistered.
    pub fn remove(&self, path: &str) -> Result<Vec<String>, String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| format!("bad plugin path: {e}"))?
            .to_string_lossy()
            .to_string();

        let mut map = self.plugins.write().map_err(|e| e.to_string())?;
        match map.remove(&canonical) {
            Some(entry) => Ok(entry.tool_names),
            None => Err(format!("plugin not found: {canonical}")),
        }
    }

    /// Reload a plugin from disk (re‑initialize the WASM module).
    pub fn reload(&self, path: &str) -> Result<Vec<ToolDef>, Box<dyn std::error::Error>> {
        let canonical = std::fs::canonicalize(path)?;
        let canonical_str = canonical.to_string_lossy().to_string();
        let _ = self.plugins.write().unwrap().remove(&canonical_str);
        self.add(&canonical_str)
    }

    /// List all currently loaded plugins.
    pub fn list(&self) -> Vec<Info> {
        let map = self.plugins.read().unwrap();
        map.values()
            .map(|entry| Info {
                path: entry.canonical_path.clone(),
                version: entry.version,
                tools: entry.tool_names.clone(),
            })
            .collect()
    }

    /// Lookup the ext_name for a plugin by its canonical path.
    pub fn get_ext_name(&self, canonical_path: &str) -> Option<String> {
        let map = self.plugins.read().ok()?;
        map.get(canonical_path).map(|entry| entry.ext_name.clone())
    }

    /// Lookup an `Arc` to a plugin by its canonical path.
    pub fn get_plugin(&self, canonical_path: &str) -> Option<Arc<Mutex<Extension>>> {
        let map = self.plugins.read().ok()?;
        map.get(canonical_path).map(|entry| entry.plugin.clone())
    }
}

// ---------------------------------------------------------------------------
// ToolAdapter — routes LLM tool calls to WASM plugin execution
// ---------------------------------------------------------------------------

/// A tool whose execution is routed back to a WASM plugin via [`Registry`].
///
/// Before each execution the shared context (`registry.ctx`) is merged with
/// the tool‑specific `ext_name` and injected into the WASM store, so that
/// data‑oriented host functions (write/read/delete/list) can compute paths.
pub struct ToolAdapter {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// Canonical path of the `.wasm` file (used as the registry key).
    pub plugin_path: String,
    /// Extension name derived from the plugin path (used for data dirs).
    pub ext_name: String,
    /// Shared registry to look up the current plugin instance and context.
    pub registry: Arc<Registry>,
}

#[async_trait]
impl Tool for ToolAdapter {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(&self, args: serde_json::Value, _rt: &dyn crate::runtime::Runtime) -> AgentResult<String> {
        let plugin_arc = self.registry.get_plugin(&self.plugin_path)
            .ok_or_else(|| AgentError::Tool("plugin no longer loaded".into()))?;

        let mut plugin = plugin_arc.lock().map_err(|e| AgentError::Tool(e.to_string()))?;

        // Inject context into the WASM store so data host functions can path‑resolve
        let reg_ctx = self.registry.ctx.read().map_err(|e| AgentError::Tool(e.to_string()))?;
        let exec_ctx = make_exec_context(&reg_ctx, &self.ext_name);
        drop(reg_ctx);
        plugin.set_context(&exec_ctx);

        plugin.execute_tool(&self.name, &args.to_string())
            .map_err(|e| AgentError::Tool(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// WASM memory helpers
// ---------------------------------------------------------------------------

type WasmCaller<'a> = wasmtime::Caller<'a, Context>;

fn mem_get(caller: &mut WasmCaller) -> Option<wasmtime::Memory> {
    match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => Some(m),
        _ => None,
    }
}

fn mem_read_str(caller: &mut WasmCaller, ptr: u32, len: u32) -> String {
    if len == 0 { return String::new(); }
    let mem = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => m,
        _ => return String::new(),
    };
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf).ok();
    String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string()
}

fn mem_read_bytes(caller: &mut WasmCaller, ptr: u32, len: u32) -> Vec<u8> {
    if len == 0 { return Vec::new(); }
    let mem = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => m,
        _ => return Vec::new(),
    };
    let mut buf = vec![0u8; len as usize];
    mem.read(caller, ptr as usize, &mut buf).ok();
    buf
}

// ---------------------------------------------------------------------------
// Host data dimension registration
// ---------------------------------------------------------------------------

/// Register the four data‑oriented host functions for a single dimension.
///
/// Each dimension (global / project / project_local / session) gets:
/// - `host_write_{dim}_data(key_ptr, key_len, data_ptr, data_len) -> u32`
/// - `host_read_{dim}_data(key_ptr, key_len, out_buf, out_capacity) -> u32`
/// - `host_delete_{dim}_data(key_ptr, key_len) -> u32`
/// - `host_list_{dim}_data(out_buf, out_capacity) -> u32`
fn register_dim(
    linker: &mut Linker<Context>,
    dim_name: String,
    dir_fn: fn(&Context) -> std::path::PathBuf,
) -> Result<(), wasmtime::Error> {
    let dim_name_write = dim_name.clone();
    let dim_name_read = dim_name.clone();
    let dim_name_delete = dim_name.clone();
    let dim_name_list = dim_name;

    // ── write ─────────────────────────────────────────────────────────────
    let fname_w = format!("host_write_{dim_name_write}_data");
    linker.func_wrap("env", &fname_w,
        move |mut caller: WasmCaller,
              key_ptr: u32, key_len: u32,
              data_ptr: u32, data_len: u32| -> u32 {
            let ctx = caller.data().clone();
            let dir = dir_fn(&ctx);
            let key = mem_read_str(&mut caller, key_ptr, key_len);
            let data = mem_read_bytes(&mut caller, data_ptr, data_len);
            if !dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    tracing::warn!("[plugin] write:{dim_name_write} mkdir failed: {e}");
                    return 1;
                }
            }
            let tmp = dir.join(format!("{key}.tmp"));
            let final_path = dir.join(&key);
            if std::fs::write(&tmp, &data).is_err() { return 1; }
            if std::fs::rename(&tmp, &final_path).is_err() { return 1; }
            0 // success
        }
    )?;

    // ── read ──────────────────────────────────────────────────────────────
    let fname_r = format!("host_read_{dim_name_read}_data");
    linker.func_wrap("env", &fname_r,
        move |mut caller: WasmCaller,
              key_ptr: u32, key_len: u32,
              out_buf: u32, out_capacity: u32| -> u32 {
            let ctx = caller.data().clone();
            let dir = dir_fn(&ctx);
            let key = mem_read_str(&mut caller, key_ptr, key_len);
            let data = match std::fs::read(dir.join(&key)) {
                Ok(d) => d,
                Err(_) => return 0, // not found
            };
            let len = data.len().min(out_capacity as usize);
            if let Some(mem) = mem_get(&mut caller) {
                mem.write(&mut caller, out_buf as usize, &data[..len]).ok();
            }
            len as u32
        }
    )?;

    // ── delete ────────────────────────────────────────────────────────────
    let fname_d = format!("host_delete_{dim_name_delete}_data");
    linker.func_wrap("env", &fname_d,
        move |mut caller: WasmCaller,
              key_ptr: u32, key_len: u32| -> u32 {
            let ctx = caller.data().clone();
            let dir = dir_fn(&ctx);
            let key = mem_read_str(&mut caller, key_ptr, key_len);
            match std::fs::remove_file(dir.join(&key)) {
                Ok(_) => 0,
                Err(_) => 1,
            }
        }
    )?;

    // ── list ──────────────────────────────────────────────────────────────
    let fname_l = format!("host_list_{dim_name_list}_data");
    linker.func_wrap("env", &fname_l,
        move |mut caller: WasmCaller,
              out_buf: u32, out_capacity: u32| -> u32 {
            let ctx = caller.data().clone();
            let dir = dir_fn(&ctx);
            let entries: Vec<String> = match std::fs::read_dir(&dir) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .filter(|e| !e.file_name().to_string_lossy().ends_with(".tmp"))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect(),
                Err(_) => return 0,
            };
            let mut joined = entries.join("\n");
            joined.push('\n');
            let bytes = joined.as_bytes();
            let len = bytes.len().min(out_capacity as usize);
            if let Some(mem) = mem_get(&mut caller) {
                mem.write(&mut caller, out_buf as usize, &bytes[..len]).ok();
            }
            len as u32
        }
    )?;

    Ok(())
}
