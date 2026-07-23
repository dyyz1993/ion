//! hello-extension — a minimal example WASM extension for ION.
//!
//! This extension demonstrates the three required entry points of the
//! ION WASM extension ABI and one lifecycle hook:
//!
//!   * `extension_version()`  → returns the ABI version (1)
//!   * `extension_init()`     → registers tools with the host
//!   * `extension_execute_tool()` → handles tool invocations
//!   * `on_input()`           → lifecycle hook (fires on every user message)
//!
//! Build:
//!   cargo build --target wasm32-wasip1 --release
//! Install:
//!   cp target/wasm32-wasip1/release/hello_extension.wasm ~/.ion/agent/extensions/

#![no_std]

// ── Host functions provided by the ION WASM runtime ─────────────────────────

extern "C" {
    /// Register a tool with the host.
    /// Arguments are raw pointer + length pairs for name, description, and
    /// a JSON-schema string describing the tool's parameters.
    fn host_register_tool(
        name_ptr: *const u8, name_len: u32,
        desc_ptr: *const u8, desc_len: u32,
        schema_ptr: *const u8, schema_len: u32,
    );

    /// Send a message to the host (printed to the event stream / stderr).
    fn host_send_message(msg_ptr: *const u8, msg_len: u32);
}

// ── Panic handler (required for #![no_std]) ─────────────────────────────────

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Thin wrapper around the raw `host_register_tool` import.
fn register_tool(name: &str, desc: &str, schema: &str) {
    unsafe {
        host_register_tool(
            name.as_ptr(), name.len() as u32,
            desc.as_ptr(), desc.len() as u32,
            schema.as_ptr(), schema.len() as u32,
        );
    }
}

/// Thin wrapper around `host_send_message`.
fn send_message(msg: &str) {
    unsafe { host_send_message(msg.as_ptr(), msg.len() as u32); }
}

/// Copy `src` into the host-provided output buffer and return the byte count.
fn write_output(src: &[u8], out_buf: *mut u8, out_capacity: u32) -> u32 {
    let len = src.len().min(out_capacity as usize);
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), out_buf, len); }
    len as u32
}

// ── ABI entry points ─────────────────────────────────────────────────────────

/// Return the extension ABI version. Must be `1`.
#[no_mangle]
pub extern "C" fn extension_version() -> u32 {
    1
}

/// Called once when the host loads the extension.
/// This is where tools are registered.
#[no_mangle]
pub extern "C" fn extension_init() {
    // Register a single tool named "hello".
    register_tool(
        "hello",
        "Returns a friendly greeting from the extension.",
        r#"{"type":"object","properties":{}}"#,
    );

    // Let the host know we loaded successfully.
    send_message("hello-extension initialized");
}

/// Called when the LLM (or RPC) invokes a tool registered by this extension.
///
/// Signature:
///   extension_execute_tool(
///       name_ptr: *const u8, name_len: u32,   // tool name
///       args_ptr: *const u8, args_len: u32,   // tool arguments (JSON)
///       out_buf:  *mut u8,    out_capacity: u32, // output buffer
///   ) -> u32  // number of bytes written to out_buf
#[no_mangle]
pub extern "C" fn extension_execute_tool(
    name_ptr: *const u8, name_len: u32,
    _args_ptr: *const u8, _args_len: u32,
    out_buf: *mut u8, out_capacity: u32,
) -> u32 {
    // Read the tool name from the host-provided memory slice.
    let name = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len as usize))
    };

    match name {
        "hello" => {
            // The canonical greeting result.
            let result = b"{\"greeting\":\"Hello from extension!\"}";
            write_output(result, out_buf, out_capacity)
        }
        _ => {
            // Unknown tool — return a JSON error.
            write_output(b"{\"error\":\"unknown tool\"}", out_buf, out_capacity)
        }
    }
}

// ── Lifecycle hook: on_input ─────────────────────────────────────────────────

/// A-class lifecycle hook (mutable context).
///
/// The host calls `on_input` before the agent processes each user message.
/// The host passes the current input as JSON at `(json_ptr, json_len)` and
/// expects the (possibly modified) JSON to be written into `(out_buf, out_cap)`.
/// The return value is the number of bytes written.
///
/// Here we simply print a notification to stdout and pass the input through
/// unchanged so the agent sees it as-is.
#[no_mangle]
pub extern "C" fn on_input(
    json_ptr: *const u8, json_len: u32,
    out_buf: *mut u8, out_cap: u32,
) -> u32 {
    // Notify the host that the hook fired.
    send_message("hello-extension: on_input hook received user message");

    // Pass-through: copy the input JSON verbatim into the output buffer.
    let input = unsafe { core::slice::from_raw_parts(json_ptr, json_len as usize) };
    write_output(input, out_buf, out_cap)
}
