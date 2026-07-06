#![no_std]
extern crate alloc;

use alloc::string::{String, ToString};

// ── Simple bump allocator (no libc dependency) ─────────────────────────────────

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

const HEAP_SIZE: usize = 16 * 1024; // 16 KB (plan plugin barely allocates)
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

struct BumpAllocator {
    offset: core::sync::atomic::AtomicUsize,
}

impl BumpAllocator {
    const fn new() -> Self {
        Self { offset: core::sync::atomic::AtomicUsize::new(0) }
    }
}

unsafe impl core::alloc::GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        loop {
            let current = self.offset.load(core::sync::atomic::Ordering::Relaxed);
            let start = HEAP.as_ptr() as usize;
            let unaligned = start + current;
            let aligned = (unaligned + align - 1) & !(align - 1);
            let new_offset = (aligned - start) + size;
            if new_offset > HEAP_SIZE {
                return core::ptr::null_mut();
            }
            if self.offset.compare_exchange_weak(
                current, new_offset,
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
            ).is_ok() {
                core::ptr::write_bytes(aligned as *mut u8, 0, size);
                return aligned as *mut u8;
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
}

// ── Host imports ──────────────────────────────────────────────────────────────

extern "C" {
    fn host_register_tool(
        name_ptr: *const u8, name_len: u32,
        desc_ptr: *const u8, desc_len: u32,
        schema_ptr: *const u8, schema_len: u32,
    );
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

// ── Plugin exports ────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_version() -> u32 { 1 }

#[no_mangle]
pub extern "C" fn extension_init() {
    host_register(
        "plan_enter",
        "Enter planning mode. Provide a plan_path where the plan will be saved.",
        r#"{"type":"object","properties":{"plan_path":{"type":"string","description":"Path to write the plan to"}},"required":["plan_path"]}"#,
    );
    host_register(
        "plan_exit",
        "Exit planning mode and return to normal agent workflow.",
        r#"{"type":"object","properties":{}}"#,
    );
}

// ── Tool execution ────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_execute_tool(
    name_ptr: *const u8, name_len: u32,
    _args_ptr: *const u8, _args_len: u32,
    out_buf: *mut u8, out_capacity: u32,
) -> u32 {
    let name = read_str(name_ptr, name_len);
    let result = match name.as_str() {
        "plan_enter" => r#"{"status":"ok","mode":"plan"}"#,
        "plan_exit" => r#"{"status":"ok","mode":"normal"}"#,
        _ => r#"{"error":"unknown tool"}"#,
    };
    write_out(result, out_buf, out_capacity)
}

fn read_str(ptr: *const u8, len: u32) -> String {
    if len == 0 { return String::new(); }
    unsafe { String::from_utf8_lossy(core::slice::from_raw_parts(ptr, len as usize)).into_owned() }
}

fn write_out(s: &str, buf: *mut u8, capacity: u32) -> u32 {
    let bytes = s.as_bytes();
    let len = bytes.len().min(capacity as usize);
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, len); }
    len as u32
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
