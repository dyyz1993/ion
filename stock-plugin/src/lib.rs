#![no_std]

// 宿主函数（ion 提供给 WASM 插件调用）
extern "C" {
    fn host_register_tool(
        name_ptr: *const u8, name_len: u32,
        desc_ptr: *const u8, desc_len: u32,
        schema_ptr: *const u8, schema_len: u32,
    );
    fn host_send_message(msg_ptr: *const u8, msg_len: u32);
    fn host_channel_send(
        ch_ptr: *const u8, ch_len: u32,
        msg_ptr: *const u8, msg_len: u32,
    );
    fn host_create_worker(cfg_ptr: *const u8, cfg_len: u32) -> u32;
}

fn host_register(name: &str, desc: &str, schema: &str) {
    unsafe {
        host_register_tool(
            name.as_ptr(), name.len() as u32,
            desc.as_ptr(), desc.len() as u32,
            schema.as_ptr(), schema.len() as u32,
        );
    }
}

fn host_send_msg(msg: &str) {
    unsafe { host_send_message(msg.as_ptr(), msg.len() as u32); }
}

fn host_ch_send(channel: &str, msg: &str) {
    unsafe {
        host_channel_send(
            channel.as_ptr(), channel.len() as u32,
            msg.as_ptr(), msg.len() as u32,
        );
    }
}

#[no_mangle]
pub extern "C" fn plugin_version() -> u32 { 1 }

#[no_mangle]
pub extern "C" fn plugin_init() {
    host_register(
        "get_stock_price",
        "Get current stock price for a ticker symbol (e.g. AAPL)",
        r#"{"type":"object","properties":{"ticker":{"type":"string","description":"Stock ticker"}},"required":["ticker"]}"#,
    );
    // 通知宿主：插件已初始化
    host_send_msg("stock plugin initialized");
}

#[no_mangle]
pub extern "C" fn plugin_execute_tool(
    _name_ptr: *const u8, _name_len: u32,
    _args_ptr: *const u8, _args_len: u32,
    out_buf: *mut u8, out_capacity: u32,
) -> u32 {
    // 执行工具 → 返回结果
    let result = b"{\"symbol\":\"AAPL\",\"price\":198.50,\"source\":\"WASM plugin\"}";
    let len = result.len().min(out_capacity as usize);
    unsafe { core::ptr::copy_nonoverlapping(result.as_ptr(), out_buf, len); }
    
    // 通过宿主 API 广播到 channel（演示 host_channel_send）
    host_ch_send("stock-updates", r#"{"symbol":"AAPL","price":198.50}"#);
    
    len as u32
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
