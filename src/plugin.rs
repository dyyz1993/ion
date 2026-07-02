use std::collections::HashMap;
use std::path::Path;
use wasmtime::{Engine, Linker, Memory, MemoryType, Module, Store};

/// A loaded WASM plugin instance with registered tools.
pub struct WasmPlugin {
    engine: Engine,
    store: Store<()>,
    instance: wasmtime::Instance,
    memory: Memory,
    /// Tools registered by the plugin during init.
    pub tools: Vec<PluginToolDef>,
}

#[derive(Clone)]
pub struct PluginToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl WasmPlugin {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);

        let memory = Memory::new(&mut store, MemoryType::new(1, None))?;
        let tools_registered = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tools = tools_registered.clone();

        linker.func_wrap("env", "host_register_tool", 
            move |mut caller: wasmtime::Caller<'_, ()>,
                  name_ptr: u32, name_len: u32,
                  desc_ptr: u32, desc_len: u32,
                  schema_ptr: u32, schema_len: u32| {
                let mem = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(m)) => m,
                    _ => { return; }
                };
                let read_str = |ptr: u32, len: u32| -> String {
                    if len == 0 { return String::new(); }
                    let mut buf = vec![0u8; len as usize];
                    mem.read(&caller, ptr as usize, &mut buf).ok();
                    String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string()
                };
                let name = read_str(name_ptr, name_len);
                let desc = read_str(desc_ptr, desc_len);
                let schema_str = read_str(schema_ptr, schema_len);
                let params = serde_json::from_str(&schema_str).unwrap_or_default();
                if let Ok(mut t) = tools.lock() {
                    t.push(PluginToolDef { name, description: desc, parameters: params });
                }
            }
        )?;

        linker.define(&mut store, "env", "memory", memory)?;
        let instance = linker.instantiate(&mut store, &module)?;

        let mut plugin = Self {
            engine,
            store,
            instance,
            memory,
            tools: Vec::new(),
        };

        // Call plugin_version
        if let Ok(func) = plugin.instance.get_typed_func::<(), u32>(&mut plugin.store, "plugin_version") {
            if let Ok(ver) = func.call(&mut plugin.store, ()) {
                tracing::info!("[wasm] plugin v{ver}");
            }
        }

        // Call plugin_init — this triggers host_register_tool callbacks
        if let Ok(func) = plugin.instance.get_typed_func::<(), ()>(&mut plugin.store, "plugin_init") {
            func.call(&mut plugin.store, ())?;
        }

        // Drop linker so the Arc refcount drops to 1
        drop(linker);

        // Collect registered tools
        if let Ok(t) = tools_registered.lock() {
            plugin.tools = t.clone();
        }
        tracing::info!("[wasm] plugin registered {} tools", plugin.tools.len());

        Ok(plugin)
    }

    pub fn execute_tool(&mut self, name: &str, args: &str) -> Result<String, Box<dyn std::error::Error>> {
        let func = self.instance.get_typed_func::<(u32, u32, u32, u32, u32, u32), u32>(
            &mut self.store, "plugin_execute_tool"
        )?;

        let name_bytes = name.as_bytes();
        let args_bytes = args.as_bytes();
        let name_offset = 0u32;
        let name_len = name_bytes.len() as u32;
        let args_offset = name_len;
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

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;

/// A tool whose execution is routed back to a WASM plugin.
pub struct WasmCallingTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub plugin: Arc<Mutex<WasmPlugin>>,
}

#[async_trait]
impl Tool for WasmCallingTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(&self, args: serde_json::Value) -> AgentResult<String> {
        let args_str = args.to_string();
        let name = self.name.clone();
        let mut plugin = self.plugin.lock().map_err(|e| AgentError::Tool(e.to_string()))?;
        plugin.execute_tool(&name, &args_str)
            .map_err(|e| AgentError::Tool(e.to_string()))
    }
}
