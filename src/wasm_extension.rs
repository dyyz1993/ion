use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use wasmtime::{Engine, Linker, Memory, MemoryType, Module, Store};

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;
use crate::event_bus::ExtensionEvent;
use crate::paths;
use crate::runtime::pending_ui;

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
#[derive(Clone)]
pub struct Context {
    /// Current session ID (used for session-level data paths).
    pub session_id: String,
    /// Current working directory (used for session/project paths).
    pub cwd: String,
    /// Project root directory (used for project-local paths).
    pub project_root: String,
    /// Extension name (the subdirectory name inside each data dir).
    pub ext_name: String,
    /// EventBus for UI events (host_ui_ask/confirm/notif/alert/prompt).
    pub event_bus: Option<std::sync::Arc<tokio::sync::Mutex<crate::event_bus::ExtensionEventBus>>>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("project_root", &self.project_root)
            .field("ext_name", &self.ext_name)
            .finish()
    }
}

impl Default for Context {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cwd: String::new(),
            project_root: String::new(),
            ext_name: String::new(),
            event_bus: None,
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
                "event": {"type": "custom", "customType": "extension_message",
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

	    // ── Host: ui_ask ──────────────────────────────────────────────────
	    linker.func_wrap("env", "host_ui_ask",
	        move |mut caller: wasmtime::Caller<'_, Context>,
	              title_ptr: u32, title_len: u32,
	              msg_ptr: u32, msg_len: u32| -> u32 {
	            let title = mem_read_str(&mut caller, title_ptr, title_len);
	            let message = mem_read_str(&mut caller, msg_ptr, msg_len);
	            let ctx = caller.data().clone();
	            let bus = match ctx.event_bus {
				Some(ref b) => b.clone(),
				None => return 0, // no event bus — deny
			    };
	            let request_id = format!("req_{}", &uuid::Uuid::new_v4().to_string()[..8]);

	            // register pending
	            let (tx, rx) = oneshot::channel();
	            pending_ui().lock().unwrap().insert(request_id.clone(), tx);

	            // push Ask event
	            let event = ExtensionEvent::new_ui("Ask", &title, &message)
	                .with_data(serde_json::json!({"request_id": request_id, "title": title, "message": message}));
	            {
	                let mut b = bus.blocking_lock();
	                b.broadcast(&event);
	            }

	            // wait for response (blocking — we're in a sync closure)
	            match rx.blocking_recv() {
	                Ok(resp) if resp == "allow" => 1,
	                _ => 0,
	            }
	        }
	    )?;

	    // ── Host: ui_confirm ───────────────────────────────────────────────
	    linker.func_wrap("env", "host_ui_confirm",
	        move |mut caller: wasmtime::Caller<'_, Context>,
	              title_ptr: u32, title_len: u32,
	              msg_ptr: u32, msg_len: u32| -> u32 {
	            let title = mem_read_str(&mut caller, title_ptr, title_len);
	            let message = mem_read_str(&mut caller, msg_ptr, msg_len);
	            let ctx = caller.data().clone();
	            let bus = match ctx.event_bus {
				Some(ref b) => b.clone(),
				None => return 0,
			    };
	            let request_id = format!("req_{}", &uuid::Uuid::new_v4().to_string()[..8]);

	            let (tx, rx) = oneshot::channel();
	            pending_ui().lock().unwrap().insert(request_id.clone(), tx);

	            let event = ExtensionEvent::new_ui("Confirm", &title, &message)
	                .with_data(serde_json::json!({"request_id": request_id, "title": title, "message": message}));
	            {
	                let mut b = bus.blocking_lock();
	                b.broadcast(&event);
	            }

	            match rx.blocking_recv() {
	                Ok(resp) if resp == "confirm" => 1,
	                _ => 0,
	            }
	        }
	    )?;

	    // ── Host: ui_notif ────────────────────────────────────────────────
	    linker.func_wrap("env", "host_ui_notif",
	        move |mut caller: wasmtime::Caller<'_, Context>,
	              title_ptr: u32, title_len: u32,
	              msg_ptr: u32, msg_len: u32| {
	            let title = mem_read_str(&mut caller, title_ptr, title_len);
	            let message = mem_read_str(&mut caller, msg_ptr, msg_len);
	            if let Some(ref bus) = caller.data().event_bus {
	                let event = ExtensionEvent::new_ui("Notif", &title, &message);
	                let mut b = bus.blocking_lock();
	                b.broadcast(&event);
	            }
	        }
	    )?;

	    // ── Host: ui_alert ────────────────────────────────────────────────
	    linker.func_wrap("env", "host_ui_alert",
	        move |mut caller: wasmtime::Caller<'_, Context>,
	              title_ptr: u32, title_len: u32,
	              msg_ptr: u32, msg_len: u32| {
	            let title = mem_read_str(&mut caller, title_ptr, title_len);
	            let message = mem_read_str(&mut caller, msg_ptr, msg_len);
	            if let Some(ref bus) = caller.data().event_bus {
	                let event = ExtensionEvent::new_ui("Alert", &title, &message)
	                    .with_data(serde_json::json!({"level": "warning"}));
	                let mut b = bus.blocking_lock();
	                b.broadcast(&event);
	            }
	        }
	    )?;

	    // ── Host: ui_prompt ───────────────────────────────────────────────
	    linker.func_wrap("env", "host_ui_prompt",
	        move |mut caller: wasmtime::Caller<'_, Context>,
	              title_ptr: u32, title_len: u32,
	              msg_ptr: u32, msg_len: u32,
	              out_buf: u32, out_capacity: u32| -> u32 {
	            let title = mem_read_str(&mut caller, title_ptr, title_len);
	            let message = mem_read_str(&mut caller, msg_ptr, msg_len);
	            let ctx = caller.data().clone();
	            let bus = match ctx.event_bus {
				Some(ref b) => b.clone(),
				None => return 0,
			    };
	            let request_id = format!("req_{}", &uuid::Uuid::new_v4().to_string()[..8]);

	            let (tx, rx) = oneshot::channel();
	            pending_ui().lock().unwrap().insert(request_id.clone(), tx);

	            let event = ExtensionEvent::new_ui("Prompt", &title, &message)
	                .with_data(serde_json::json!({"request_id": request_id, "title": title, "message": message}));
	            {
	                let mut b = bus.blocking_lock();
	                b.broadcast(&event);
	            }

	            match rx.blocking_recv() {
	                Ok(resp) => {
	                    let bytes = resp.as_bytes();
	                    let len = bytes.len().min(out_capacity as usize);
	                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
	                        mem.write(&mut caller, out_buf as usize, &bytes[..len]).ok();
	                    }
	                    len as u32
	                }
	                _ => 0,
	            }
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

        let mut ext = Self {
            engine,
            store,
            instance,
            memory: instance_memory,
            tools: Vec::new(),
            version: 0,
        };

        // Call version (extension_version or legacy plugin_version)
        if let Ok(func) =  ext.get_export_func::<(), u32>("version") {
            if let Ok(ver) = func.call(&mut ext.store, ()) {
                ext.version = ver;
                tracing::info!("[wasm] extension v{ver}");
            }
        }

        // Call init (extension_init or legacy plugin_init) — triggers host_register_tool
        if let Ok(func) =  ext.get_export_func::<(), ()>("init") {
            func.call(&mut ext.store, ())?;
        } else {
        }

        drop(linker);

        // Collect registered tools
        if let Ok(t) = tools_registered.lock() {
            ext.tools = t.clone();
        } else {
        }
        let tool_names: Vec<&str> = ext.tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!("[wasm] extension v{} registered {} tools: {:?}", ext.version, ext.tools.len(), tool_names);

        Ok(ext)
    }

    /// Inject a new context into the WASM store (called before tool execution).
    pub fn set_context(&mut self, ctx: &Context) {
        *self.store.data_mut() = ctx.clone();
    }

    /// Look up a typed export by short name.
    /// Resolves to `extension_<short_name>` — WASM modules must export with
    /// the `extension_` prefix.
    fn get_export_func<Params, Results>(
        &mut self,
        short_name: &str,
    ) -> Result<wasmtime::TypedFunc<Params, Results>, wasmtime::Error>
    where
        Params: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        let name = format!("extension_{}", short_name);
        self.instance.get_typed_func::<Params, Results>(&mut self.store, &name)
    }

    pub fn execute_tool(&mut self, name: &str, args: &str) -> Result<String, Box<dyn std::error::Error>> {
        let func = self.get_export_func::<(u32, u32, u32, u32, u32, u32), u32>("execute_tool")?;

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

    /// Call a B-class hook: one-way notification, no return data.
    /// WASM export: `extension_on_<hook>` (legacy: `plugin_on_<hook>`).
    /// Returns Ok(false) if the hook is not exported (extension chose not to implement it).
    pub fn call_hook_notify(&mut self, hook: &str, ctx_json: &str) -> Result<bool, String> {
        let short = format!("on_{}", hook);
        let func = match self.get_export_func::<(u32, u32), ()>(&short) {
            Ok(f) => f,
            Err(_) => return Ok(false), // hook not exported
        };
        let json_bytes = ctx_json.as_bytes();
        let json_len = json_bytes.len() as u32;
        self.memory
            .write(&mut self.store, DATA_OFFSET as usize, json_bytes)
            .map_err(|e| format!("mem write: {e}"))?;
        func.call(&mut self.store, (DATA_OFFSET, json_len))
            .map_err(|e| format!("wasm {}: {e}", hook))?;
        Ok(true)
    }

    /// Call an A-class hook: mutable context, WASM returns modified JSON.
    /// WASM export: `extension_on_<hook>(json_ptr, json_len, out_buf, out_cap) -> u32`.
    /// Returns None if hook not exported or WASM returned 0 bytes.
    pub fn call_hook_mut(&mut self, hook: &str, ctx_json: &str) -> Result<Option<String>, String> {
        let short = format!("on_{}", hook);
        let func = match self.get_export_func::<(u32, u32, u32, u32), u32>(&short) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let json_bytes = ctx_json.as_bytes();
        let json_len = json_bytes.len() as u32;
        let out_offset = DATA_OFFSET + json_len + 4;
        let out_capacity: u32 = 65_536;

        self.memory
            .write(&mut self.store, DATA_OFFSET as usize, json_bytes)
            .map_err(|e| format!("mem write: {e}"))?;

        let result_len = func
            .call(&mut self.store, (DATA_OFFSET, json_len, out_offset, out_capacity))
            .map_err(|e| format!("wasm {}: {e}", hook))?;

        if result_len == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; result_len as usize];
        self.memory
            .read(&mut self.store, out_offset as usize, &mut buf)
            .map_err(|e| format!("mem read: {e}"))?;
        Ok(Some(String::from_utf8_lossy(&buf).to_string()))
    }

    /// Call a C-class hook: returns a status code (0 or non-zero).
    /// Used by `before_tool_call` where 0=allow, non-zero=block.
    /// WASM export: `extension_<hook>` (legacy: `plugin_<hook>`).
    /// Returns false if hook not exported (don't block).
    pub fn call_hook_status(&mut self, hook: &str, ctx_json: &str) -> Result<bool, String> {
        let func = match self.get_export_func::<(u32, u32), u32>(hook) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        };
        let json_bytes = ctx_json.as_bytes();
        let json_len = json_bytes.len() as u32;
        self.memory
            .write(&mut self.store, DATA_OFFSET as usize, json_bytes)
            .map_err(|e| format!("mem write: {e}"))?;
        let code = func
            .call(&mut self.store, (DATA_OFFSET, json_len))
            .map_err(|e| format!("wasm {}: {e}", hook))?;
        Ok(code != 0)
    }

    /// Call extension_rpc hook.
    /// WASM export: `extension_on_rpc` (legacy: `plugin_on_extension_rpc`).
    /// Returns Err if hook not exported or returned 0.
    pub fn call_hook_rpc(
        &mut self,
        method: &str,
        params_json: &str,
    ) -> Result<String, String> {
        let func = match self.get_export_func::<(u32, u32, u32, u32, u32, u32), u32>("on_rpc") {
            Ok(f) => f,
            Err(_) => return Err("extension rpc method not found".into()),
        };
        let method_bytes = method.as_bytes();
        let params_bytes = params_json.as_bytes();
        let method_offset = DATA_OFFSET;
        let method_len = method_bytes.len() as u32;
        let params_offset = method_offset + method_len;
        let params_len = params_bytes.len() as u32;
        let out_offset = params_offset + params_len + 4;
        let out_capacity: u32 = 65_536;

        self.memory.write(&mut self.store, method_offset as usize, method_bytes)
            .map_err(|e| format!("mem write: {e}"))?;
        self.memory.write(&mut self.store, params_offset as usize, params_bytes)
            .map_err(|e| format!("mem write: {e}"))?;

        let result_len = func.call(&mut self.store,
            (method_offset, method_len, params_offset, params_len, out_offset, out_capacity))
            .map_err(|e| format!("wasm rpc: {e}"))?;

        if result_len == 0 {
            return Err("extension rpc method not found".into());
        }
        let mut buf = vec![0u8; result_len as usize];
        self.memory.read(&mut self.store, out_offset as usize, &mut buf)
            .map_err(|e| format!("mem read: {e}"))?;
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
        .unwrap_or_else(|| "extension".into())
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
    extension: Arc<Mutex<Extension>>,
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

        let ext = Extension::load(&canonical)?;
        let version = ext.version;
        let tool_defs = ext.tools.clone();
        let tool_names: Vec<String> = tool_defs.iter().map(|t| t.name.clone()).collect();
        let ext_name = ext_name_from_path(&canonical_str);

        let entry = Entry {
            extension: Arc::new(Mutex::new(ext)),
            version,
            tool_names,
            canonical_path: canonical_str.clone(),
            ext_name,
        };

        self.plugins.write().unwrap().insert(canonical_str, entry);
        Ok(tool_defs)
    }

    /// Remessagea plugin by path. Returns the names of tools that were unregistered.
    pub fn remove(&self, path: &str) -> Result<Vec<String>, String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| format!("bad extension path: {e}"))?
            .to_string_lossy()
            .to_string();

        let mut map = self.plugins.write().map_err(|e| e.to_string())?;
        match map.remove(&canonical) {
            Some(entry) => Ok(entry.tool_names),
            None => Err(format!("extension not found: {canonical}")),
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
    pub fn get_extension(&self, canonical_path: &str) -> Option<Arc<Mutex<Extension>>> {
        let map = self.plugins.read().ok()?;
        map.get(canonical_path).map(|entry| entry.extension.clone())
    }

    /// Create a HookAdapter for a loaded WASM extension.
    ///
    /// The adapter implements the `Extension` trait, forwarding all 29 hooks
    /// to optional WASM exports (`plugin_on_*`). Hooks not exported by the
    /// WASM module are silently skipped.
    pub fn create_hook_adapter(self: &Arc<Self>, canonical_path: &str) -> Option<HookAdapter> {
        let map = self.plugins.read().ok()?;
        let entry = map.get(canonical_path)?;
        Some(HookAdapter {
            name: entry.ext_name.clone(),
            canonical_path: canonical_path.to_string(),
            registry: self.clone(),
        })
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
    pub extension_path: String,
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
        let ext_arc = self.registry.get_extension(&self.extension_path)
            .ok_or_else(|| AgentError::Tool("extension no longer loaded".into()))?;

        let mut ext = ext_arc.lock().map_err(|e| AgentError::Tool(e.to_string()))?;

        // Inject context into the WASM store so data host functions can path‑resolve
        let reg_ctx = self.registry.ctx.read().map_err(|e| AgentError::Tool(e.to_string()))?;
        let exec_ctx = make_exec_context(&reg_ctx, &self.ext_name);
        drop(reg_ctx);
        ext.set_context(&exec_ctx);

        ext.execute_tool(&self.name, &args.to_string())
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

/// 路径安全检查：确保 key 解析后仍在 dir 内（防穿越）。
///
/// 4 维数据存储 (global/project/project_local/session) 是本机内部数据，
/// 不走 Runtime trait（因为这些数据本来就只存在本机）。
/// 但必须防止 WASM 扩展用 `../` 路径穿越到 dir 之外。
///
/// 返回 Some(安全路径) 或 None(检测到穿越)。
///
/// 检查规则:
/// 1. 拒绝绝对路径 key（如 "/etc/passwd"）
/// 2. join 后规范化，必须仍在 dir 内
/// 3. 不依赖文件系统状态（用字符串级规范化，避免 TOCTOU）
fn safe_join(dir: &std::path::Path, key: &str) -> Option<std::path::PathBuf> {
    // 拒绝绝对路径
    if std::path::Path::new(key).is_absolute() {
        tracing::warn!("[extension] path traversal blocked (absolute): {}", key);
        return None;
    }
    // 拒绝含 null byte 的 key
    if key.contains('\0') {
        tracing::warn!("[extension] path traversal blocked (null byte): {}", key);
        return None;
    }
    let joined = dir.join(key);
    // 字符串级规范化（不访问 fs）：解析 . 和 ..
    let canon = canonicalize_path_str(&joined);
    // 必须以 dir 开头（dir 本身也规范化一下）
    let dir_canon = canonicalize_path_str(dir);
    if canon.starts_with(&dir_canon) {
        Some(canon)
    } else {
        tracing::warn!("[extension] path traversal blocked: {} → {} (outside {})", key, canon.display(), dir_canon.display());
        None
    }
}

/// 字符串级路径规范化（不访问文件系统）
fn canonicalize_path_str(p: &std::path::Path) -> std::path::PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        use std::path::Component;
        match comp {
            Component::CurDir => {} // 跳过 .
            Component::ParentDir => {
                // .. 弹出最后一个（除非已经在根）
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => parts.push(comp.as_os_str().to_owned()),
            Component::Normal(s) => parts.push(s.to_owned()),
        }
    }
    parts.iter().collect()
}

#[cfg(test)]
mod path_safety_tests {
    use super::*;

    #[test]
    fn normal_key_is_allowed() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(
            safe_join(dir, "mykey"),
            Some(std::path::PathBuf::from("/home/user/.ion/data/mykey"))
        );
    }

    #[test]
    fn nested_key_is_allowed() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(
            safe_join(dir, "subdir/mykey"),
            Some(std::path::PathBuf::from("/home/user/.ion/data/subdir/mykey"))
        );
    }

    #[test]
    fn parent_dir_traversal_is_blocked() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(safe_join(dir, "../../../etc/passwd"), None);
    }

    #[test]
    fn double_dot_traversal_is_blocked() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(safe_join(dir, "../secret"), None);
    }

    #[test]
    fn absolute_path_is_blocked() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(safe_join(dir, "/etc/passwd"), None);
    }

    #[test]
    fn null_byte_is_blocked() {
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(safe_join(dir, "key\0/etc/passwd"), None);
    }

    #[test]
    fn dot_only_key_is_allowed() {
        // 单个 . 被规范化掉，等于 dir 本身
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(
            safe_join(dir, "./mykey"),
            Some(std::path::PathBuf::from("/home/user/.ion/data/mykey"))
        );
    }

    #[test]
    fn traversal_with_subdir_is_blocked() {
        // subdir/../../etc 仍穿越
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(safe_join(dir, "subdir/../../etc/passwd"), None);
    }

    #[test]
    fn key_with_dotdot_inside_dir_stays_in_dir() {
        // a/../b → b，仍在 dir 内
        let dir = std::path::Path::new("/home/user/.ion/data");
        assert_eq!(
            safe_join(dir, "a/../b"),
            Some(std::path::PathBuf::from("/home/user/.ion/data/b"))
        );
    }

    #[test]
    fn canonicalize_path_str_resolves_dots() {
        let p = std::path::Path::new("/a/b/../c/./d");
        assert_eq!(
            canonicalize_path_str(p),
            std::path::PathBuf::from("/a/c/d")
        );
    }
}

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

            // 路径穿越检查
            let final_path = match safe_join(&dir, &key) {
                Some(p) => p,
                None => return 1, // blocked
            };
            let tmp = final_path.with_extension("tmp");

            if !dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    tracing::warn!("[extension] write:{dim_name_write} mkdir failed: {e}");
                    return 1;
                }
            }
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

            // 路径穿越检查
            let path = match safe_join(&dir, &key) {
                Some(p) => p,
                None => return 0, // blocked, treat as not found
            };
            let data = match std::fs::read(&path) {
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

            // 路径穿越检查
            let path = match safe_join(&dir, &key) {
                Some(p) => p,
                None => return 1, // blocked
            };
            match std::fs::remove_file(&path) {
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

// ===========================================================================
// HookAdapter — adapts a WASM Extension into the Extension trait (29 hooks)
// ===========================================================================

use crate::agent::agent_loop::AgentContext;
use crate::agent::extension::{
    Extension as ExtTrait, BeforeAgentContext, InputContext,
    ModelSelectContext, ProviderRequestContext, ProviderResponseContext,
    SessionContext, ToolExecutionContext, TurnContext,
};
use crate::agent::messages::Message;
use ion_provider::types::{ToolCall, ToolResult, Usage};

/// Adapts a loaded WASM Extension into the `Extension` trait.
///
/// Each hook checks whether the WASM module exports the corresponding
/// `plugin_on_<hook>` function. If it does, the hook serializes its context
/// to JSON, writes it into WASM linear memory, calls the export, and (for
/// mutable hooks) reads back the modified JSON.
///
/// Hooks that the WASM module does NOT export are silently skipped
/// (return `Ok(())`), so a WASM extension only needs to export the hooks
/// it cares about.
pub struct HookAdapter {
    /// Extension name (used for extension_rpc routing + data dirs).
    pub name: String,
    /// Canonical path of the `.wasm` file (registry key).
    pub canonical_path: String,
    /// Shared registry (for context lookup).
    pub registry: Arc<Registry>,
}

impl HookAdapter {
    /// Lock the WASM Extension and inject the current session context.
    fn with_plugin<F, R>(&self, f: F) -> AgentResult<R>
    where
        F: FnOnce(&mut Extension) -> R,
    {
        let ext_arc = self.registry
            .get_extension(&self.canonical_path)
            .ok_or_else(|| AgentError::Tool(format!("extension no longer loaded: {}", self.canonical_path)))?;
        let mut ext = ext_arc
            .lock()
            .map_err(|e| AgentError::Tool(e.to_string()))?;
        // Inject context
        let reg_ctx = self
            .registry
            .ctx
            .read()
            .map_err(|e| AgentError::Tool(e.to_string()))?;
        let exec_ctx = make_exec_context(&reg_ctx, &self.name);
        drop(reg_ctx);
        ext.set_context(&exec_ctx);
        Ok(f(&mut ext))
    }

    /// B-class: one-way notification. No-op if hook not exported.
    fn notify(&self, hook: &str, ctx_json: &serde_json::Value) -> AgentResult<()> {
        let json_str = serde_json::to_string(ctx_json).unwrap_or_default();
        let result: Result<bool, String> = self.with_plugin(|p| p.call_hook_notify(hook, &json_str))?;
        result.map_err(AgentError::Tool)?;
        Ok(())
    }

    /// A-class: mutable context. Returns modified JSON or None.
    fn call_mut(&self, hook: &str, ctx_json: &serde_json::Value) -> AgentResult<Option<serde_json::Value>> {
        let json_str = serde_json::to_string(ctx_json).unwrap_or_default();
        let result: Result<Option<String>, String> = self.with_plugin(|p| p.call_hook_mut(hook, &json_str))?;
        match result.map_err(AgentError::Tool)? {
            None => Ok(None),
            Some(s) => {
                let v = serde_json::from_str(&s)
                    .map_err(|e| AgentError::Tool(format!("wasm {} bad json: {e}", hook)))?;
                Ok(Some(v))
            }
        }
    }

    /// C-class: boolean status. Returns false if hook not exported.
    fn call_status(&self, hook: &str, ctx_json: &serde_json::Value) -> AgentResult<bool> {
        let json_str = serde_json::to_string(ctx_json).unwrap_or_default();
        let result: Result<bool, String> = self.with_plugin(|p| p.call_hook_status(hook, &json_str))?;
        result.map_err(AgentError::Tool)
    }
}

#[async_trait]
impl ExtTrait for HookAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    // ── Session lifecycle ──
    async fn on_session_start(&self, ctx: &SessionContext) -> AgentResult<()> {
        self.notify("on_session_start", &serde_json::json!({"reason": &ctx.reason}))
    }

    async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
        self.notify("on_session_shutdown", &serde_json::json!({"reason": &ctx.reason}))
    }

    async fn on_session_before_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        let result = self.call_mut("on_session_before_compact", &serde_json::json!({"messages": msgs}))?;
        if let Some(v) = result {
            if let Some(m) = v.get("messages") {
                if let Some(new_msgs) = serde_json::from_value::<Vec<Message>>(m.clone()).ok() {
                    *msgs = new_msgs;
                }
            }
        }
        Ok(())
    }

    async fn on_session_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        let result = self.call_mut("on_session_compact", &serde_json::json!({"messages": msgs}))?;
        if let Some(v) = result {
            if let Some(m) = v.get("messages") {
                if let Some(new_msgs) = serde_json::from_value::<Vec<Message>>(m.clone()).ok() {
                    *msgs = new_msgs;
                }
            }
        }
        Ok(())
    }

    // ── Input ──
    async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
        let input = serde_json::json!({"text": &ctx.text, "handled": ctx.handled});
        let result = self.call_mut("on_input", &input)?;
        if let Some(v) = result {
            if let Some(h) = v.get("handled").and_then(|v| v.as_bool()) {
                ctx.handled = h;
            }
            if let Some(t) = v.get("text").and_then(|v| v.as_str()) {
                ctx.text = t.to_string();
            }
        }
        Ok(())
    }

    // ── Agent lifecycle ──
    async fn before_agent_start(&self, ctx: &mut BeforeAgentContext) -> AgentResult<()> {
        let payload = serde_json::json!({
            "system_prompt": &ctx.system_prompt,
            "messages": &ctx.messages,
        });
        let result = self.call_mut("before_agent_start", &payload)?;
        if let Some(v) = result {
            if let Some(sp) = v.get("system_prompt").and_then(|v| v.as_str()) {
                ctx.system_prompt = Some(sp.to_string());
            }
            if let Some(m) = v.get("messages") {
                if let Some(new_msgs) = serde_json::from_value::<Vec<Message>>(m.clone()).ok() {
                    ctx.messages = new_msgs;
                }
            }
        }
        Ok(())
    }

    async fn on_agent_start(&self, ctx: &AgentContext) -> AgentResult<()> {
        self.notify("on_agent_start", &serde_json::json!({
            "turn_index": ctx.turn_index,
            "message_count": ctx.message_count,
            "tool_call_count": ctx.tool_call_count,
        }))
    }

    async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
        self.notify("on_agent_end", &serde_json::json!({
            "turn_index": ctx.turn_index,
            "message_count": ctx.message_count,
            "tool_call_count": ctx.tool_call_count,
        }))
    }

    // ── Turn lifecycle ──
    async fn on_turn_start(&self, ctx: &mut TurnContext) -> AgentResult<()> {
        let payload = serde_json::json!({
            "turn_index": ctx.turn_index,
            "messages": &ctx.messages,
            "has_tool_calls": ctx.has_tool_calls,
            "stop_reason": &ctx.stop_reason,
        });
        let result = self.call_mut("on_turn_start", &payload)?;
        if let Some(v) = result {
            if let Some(m) = v.get("messages") {
                if let Some(new_msgs) = serde_json::from_value::<Vec<Message>>(m.clone()).ok() {
                    ctx.messages = new_msgs;
                }
            }
        }
        Ok(())
    }

    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        self.notify("on_turn_end", &serde_json::json!({
            "turn_index": ctx.turn_index,
            "has_tool_calls": ctx.has_tool_calls,
            "stop_reason": &ctx.stop_reason,
        }))
    }

    // ── Context / Provider ──
    async fn on_context(&self, messages: &mut Vec<Message>) -> AgentResult<()> {
        let result = self.call_mut("on_context", &serde_json::json!({"messages": messages}))?;
        if let Some(v) = result {
            if let Some(m) = v.get("messages") {
                if let Some(new_msgs) = serde_json::from_value::<Vec<Message>>(m.clone()).ok() {
                    *messages = new_msgs;
                }
            }
        }
        Ok(())
    }

    async fn before_provider_request(&self, ctx: &ProviderRequestContext) -> AgentResult<()> {
        self.notify("before_provider_request", &serde_json::json!({
            "model": &ctx.model,
            "provider": &ctx.provider,
        }))
    }

    async fn after_provider_response(&self, ctx: &ProviderResponseContext) -> AgentResult<()> {
        self.notify("after_provider_response", &serde_json::json!({
            "model": &ctx.model,
            "provider": &ctx.provider,
            "status": ctx.status,
        }))
    }

    // ── Streaming ──
    async fn on_message_start(&self, role: &str, content: &str) -> AgentResult<()> {
        self.notify("on_message_start", &serde_json::json!({"role": role, "content": content}))
    }

    async fn on_message_delta(&self, delta: &str, role: &str) -> AgentResult<()> {
        self.notify("on_message_delta", &serde_json::json!({"delta": delta, "role": role}))
    }

    async fn on_message_end(&self, role: &str, full_content: &str, usage: &Usage) -> AgentResult<()> {
        self.notify("on_message_end", &serde_json::json!({
            "role": role,
            "content": full_content,
            "usage": {"input": usage.input, "output": usage.output, "total_tokens": usage.total_tokens},
        }))
    }

    async fn on_thinking_delta(&self, delta: &str) -> AgentResult<()> {
        self.notify("on_thinking_delta", &serde_json::json!({"delta": delta}))
    }

    async fn on_thinking_end(&self, content: &str) -> AgentResult<()> {
        self.notify("on_thinking_end", &serde_json::json!({"content": content}))
    }

    async fn on_tool_call_delta(&self, delta: &str, name: &str) -> AgentResult<()> {
        self.notify("on_tool_call_delta", &serde_json::json!({"delta": delta, "name": name}))
    }

    async fn on_text_end(&self, content: &str) -> AgentResult<()> {
        self.notify("on_text_end", &serde_json::json!({"content": content}))
    }

    async fn on_tool_call_end(&self, tool_call: &ToolCall) -> AgentResult<()> {
        self.notify("on_tool_call_end", &serde_json::json!({
            "id": &tool_call.id,
            "name": &tool_call.name,
            "arguments": &tool_call.arguments,
        }))
    }

    // ── Tool execution ──
    async fn on_tool_execution_start(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        self.notify("on_tool_execution_start", &serde_json::json!({
            "tool_call_id": &ctx.tool_call_id,
            "tool_name": &ctx.tool_name,
            "is_error": ctx.is_error,
        }))
    }

    async fn on_tool_execution_update(&self, ctx: &ToolExecutionContext, partial: &str) -> AgentResult<()> {
        self.notify("on_tool_execution_update", &serde_json::json!({
            "tool_call_id": &ctx.tool_call_id,
            "tool_name": &ctx.tool_name,
            "partial": partial,
        }))
    }

    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        self.notify("on_tool_execution_end", &serde_json::json!({
            "tool_call_id": &ctx.tool_call_id,
            "tool_name": &ctx.tool_name,
            "is_error": ctx.is_error,
            "duration_ms": ctx.duration_ms,
        }))
    }

    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        let blocked = self.call_status("before_tool_call", &serde_json::json!({
            "id": &call.id,
            "name": &call.name,
            "arguments": &call.arguments,
        }))?;
        if blocked {
            return Err(AgentError::Tool(format!(
                "blocked by extension '{}'",
                self.name
            )));
        }
        Ok(())
    }

    async fn after_tool_call(&self, call: &ToolCall, result: &ToolResult) -> AgentResult<()> {
        self.notify("after_tool_call", &serde_json::json!({
            "id": &call.id,
            "name": &call.name,
            "result": &result.output,
        }))
    }

    // ── Model ──
    async fn on_model_select(&self, ctx: &ModelSelectContext) -> AgentResult<()> {
        self.notify("on_model_select", &serde_json::json!({
            "old_model": &ctx.old_model,
            "new_model": &ctx.new_model,
            "new_provider": &ctx.new_provider,
        }))
    }

    async fn on_thinking_level_select(&self, level: &str, old: Option<&str>) -> AgentResult<()> {
        self.notify("on_thinking_level_select", &serde_json::json!({
            "level": level,
            "old": old,
        }))
    }

    // ── Entries ──
    async fn on_entries_invalidated(&self, entry_ids: &[String]) -> AgentResult<()> {
        self.notify("on_entries_invalidated", &serde_json::json!({"entry_ids": entry_ids}))
    }

    // ── Session navigation ──
    async fn on_session_before_switch(&self, ctx: &super::agent::extension::SessionSwitchContext) -> AgentResult<()> {
        self.notify("on_session_before_switch", &serde_json::json!({"target_leaf_id": ctx.target_leaf_id, "source_leaf_id": ctx.source_leaf_id, "branch_name": ctx.branch_name}))
    }

    async fn on_session_before_fork(&self, ctx: &super::agent::extension::SessionSwitchContext) -> AgentResult<()> {
        self.notify("on_session_before_fork", &serde_json::json!({"target_leaf_id": ctx.target_leaf_id, "source_leaf_id": ctx.source_leaf_id, "branch_name": ctx.branch_name}))
    }

    async fn on_session_before_tree(&self, target: &str) -> AgentResult<()> {
        self.notify("on_session_before_tree", &serde_json::json!({"target": target}))
    }

    async fn on_session_tree(&self, leaf_id: &str) -> AgentResult<()> {
        self.notify("on_session_tree", &serde_json::json!({"leaf_id": leaf_id}))
    }

    // ── Extension RPC ──
    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        let params_str = serde_json::to_string(&params).unwrap_or_default();
        let result: Result<String, String> = self.with_plugin(|p| {
            p.call_hook_rpc(method, &params_str)
        })?;
        let result_str = result.map_err(AgentError::Tool)?;
        let result_json = serde_json::from_str(&result_str)
            .map_err(|e| AgentError::Tool(format!("wasm rpc bad json: {e}")))?;
        Ok(result_json)
    }

    // ── Permission ──
    async fn on_permission_request(&self, tool: &str, args: &serde_json::Value) -> AgentResult<()> {
        self.notify("on_permission_request", &serde_json::json!({"tool": tool, "args": args}))
    }

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        let result = self.call_mut("on_system_prompt", &serde_json::json!({"prompt": &*prompt}))?;
        if let Some(v) = result {
            if let Some(p) = v.get("prompt").and_then(|v| v.as_str()) {
                *prompt = p.to_string();
            }
        }
        Ok(())
    }
}
