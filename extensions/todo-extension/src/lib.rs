//! TODO Extension -- session-scoped task management.
//!
//! Data is stored in session-scoped storage at
//!   `~/.ion/agent/sessions/--hash--name--/data/{session_id}/todo_extension/tasks`
//! Format: a JSON array of objects: [{id, text, done, created_at}].
//!
//! Tools:
//!   todo_add(text)     -> create a task
//!   todo_list(status?) -> list tasks (all|active|done)
//!   todo_done(id)      -> mark a task done
//!   todo_remove(id)    -> delete a task
//!   todo_clean()       -> remove all done tasks
//!
//! Build:
//!   cargo build --target wasm32-wasip1 --release
//!   cp target/wasm32-wasip1/release/todo_extension.wasm <project>/.ion/extensions/

#![no_std]

// ── Host functions (provided by the ION worker) ─────────────────────────────

#[link(wasm_import_module = "env")]
extern "C" {
    // Tool registration.
    fn host_register_tool(
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
        schema_ptr: *const u8,
        schema_len: u32,
    );
    // Session-scoped storage (path documented above).
    fn host_read_session_data(
        key_ptr: *const u8,
        key_len: u32,
        out_buf: *mut u8,
        out_capacity: u32,
    ) -> u32;
    fn host_write_session_data(
        key_ptr: *const u8,
        key_len: u32,
        data_ptr: *const u8,
        data_len: u32,
    ) -> u32;
}
// ── Panic handler (required for #![no_std]) ─────────────────────────────────

// Only compile this for the WASM target. Under `cargo test` we link against
// `std` (which provides its own panic handler), so the lang-item here would
// otherwise collide with std's.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── JSON helpers (no_std, no alloc) ─────────────────────────────────────────

/// Extract a field value from a JSON string (simplified; no nested objects).
fn json_get<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find the opening quote of a key.
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let k_start = i + 1;
        // Find the closing quote of the key.
        let k_end = json[k_start..].find('"')? + k_start;
        if &json[k_start..k_end] == key {
            // Skip past the key, the colon, and any whitespace.
            let mut v = k_end + 1;
            while v < bytes.len()
                && (bytes[v] == b'"' || bytes[v] == b':' || bytes[v] == b' ' || bytes[v] == b'\t')
            {
                v += 1;
            }
            let val_start = v;
            if bytes.get(val_start)? == &b'"' {
                // String value: return the bytes between the quotes.
                let content_start = val_start + 1;
                let end = json[content_start..].find('"')? + content_start;
                return Some(&json[content_start..end]);
            } else {
                // Number or boolean: return the bare token.
                let mut end = val_start;
                while end < bytes.len()
                    && bytes[end] != b','
                    && bytes[end] != b'}'
                    && bytes[end] != b' '
                {
                    if bytes[end] == b'"' {
                        break;
                    }
                    end += 1;
                }
                return Some(&json[val_start..end]);
            }
        }
        // Skip to the end of this value before looking for the next key.
        let val_start = json[k_end + 1..].find(':')? + k_end + 2;
        // Handle nested objects/arrays by counting braces.
        if bytes.get(val_start)? == &b'{' || bytes.get(val_start)? == &b'[' {
            let mut depth = 1;
            let mut j = val_start + 1;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'{' || bytes[j] == b'[' {
                    depth += 1;
                } else if bytes[j] == b'}' || bytes[j] == b']' {
                    depth -= 1;
                }
                j += 1;
            }
            i = j;
        } else if bytes.get(val_start)? == &b'"' {
            let end = json[val_start + 1..].find('"')? + val_start + 2;
            i = end;
        } else {
            let mut end = val_start;
            while end < bytes.len()
                && bytes[end] != b','
                && bytes[end] != b'}'
                && bytes[end] != b']'
            {
                end += 1;
            }
            i = end;
        }
    }
    None
}

/// Return the byte offset one past the end of the Nth top-level object.
fn json_skip_objects(json: &str, count: usize) -> usize {
    let bytes = json.as_bytes();
    let mut i = 0;
    let mut found = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'{' {
                    depth += 1;
                } else if bytes[j] == b'}' {
                    depth -= 1;
                }
                j += 1;
            }
            if found >= count {
                return j;
            }
            found += 1;
            i = j;
        } else {
            i += 1;
        }
    }
    bytes.len()
}

// ── Output buffer helper ────────────────────────────────────────────────────

struct Buf<'a>(&'a mut [u8], usize);

impl Buf<'_> {
    fn s(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.b(b);
        }
    }
    fn b(&mut self, b: u8) {
        if self.1 < self.0.len() {
            self.0[self.1] = b;
            self.1 += 1;
        }
    }
    fn num(&mut self, n: u64) {
        if n == 0 {
            return self.b(b'0');
        }
        let mut d = [0u8; 20];
        let mut i = 0;
        let mut v = n;
        while v > 0 {
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            i += 1;
        }
        while i > 0 {
            i -= 1;
            self.b(d[i]);
        }
    }
    fn esc(&mut self, s: &str) {
        self.b(b'"');
        for &b in s.as_bytes() {
            match b {
                b'"' => {
                    self.b(b'\\');
                    self.b(b'"');
                }
                b'\\' => {
                    self.b(b'\\');
                    self.b(b'\\');
                }
                b'\n' => {
                    self.b(b'\\');
                    self.b(b'n');
                }
                _ => self.b(b),
            }
        }
        self.b(b'"');
    }
    fn len(&self) -> usize {
        self.1
    }
    fn as_slice(&self) -> &[u8] {
        &self.0[..self.1]
    }
}

// ── Extension entry points ──────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn extension_init() {
    host_register(
        "todo_add",
        "Create a new task.\nArgs: {text: string}. Returns: {id, text, status}",
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
    );
    host_register(
        "todo_list", "List tasks.\nArgs: {status?: 'all'|'active'|'done'} (default 'active').\nReturns: [{id, text, done, created_at}]",
        r#"{"type":"object","properties":{"status":{"type":"string","enum":["all","active","done"]}}}"#,
    );
    host_register(
        "todo_done",
        "Mark a task done.\nArgs: {id: string}. Returns: {id, status}",
        r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}"#,
    );
    host_register(
        "todo_remove",
        "Remove a task.\nArgs: {id: string}. Returns: {id, status}",
        r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}"#,
    );
    host_register(
        "todo_clean",
        "Remove all done tasks.\nArgs: {}. Returns: {removed: count}",
        r#"{"type":"object","properties":{}}"#,
    );
}

fn host_register(name: &str, desc: &str, schema: &str) {
    unsafe {
        host_register_tool(
            name.as_ptr(),
            name.len() as u32,
            desc.as_ptr(),
            desc.len() as u32,
            schema.as_ptr(),
            schema.len() as u32,
        );
    }
}

const KEY: &str = "tasks";
const STORAGE_BUF: usize = 16384;
const OUT_BUF: usize = 4096;

#[no_mangle]
pub extern "C" fn extension_execute_tool(
    name_ptr: *const u8,
    name_len: u32,
    args_ptr: *const u8,
    args_len: u32,
    out_buf: *mut u8,
    out_capacity: u32,
) -> u32 {
    let name = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len as usize))
    };
    let args = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(args_ptr, args_len as usize))
    };

    match name {
        "todo_add" => cmd_add(args, out_buf, out_capacity),
        "todo_list" => cmd_list(args, out_buf, out_capacity),
        "todo_done" => cmd_done(args, out_buf, out_capacity),
        "todo_remove" => cmd_remove(args, out_buf, out_capacity),
        "todo_clean" => cmd_clean(args, out_buf, out_capacity),
        _ => {
            let e = b"unknown tool";
            copy_out(e, out_buf, out_capacity)
        }
    }
}

fn copy_out(src: &[u8], out: *mut u8, cap: u32) -> u32 {
    let len = src.len().min(cap as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), out, len);
    }
    len as u32
}

fn read_storage(buf: &mut [u8]) -> &str {
    let len = unsafe {
        host_read_session_data(
            KEY.as_ptr(),
            KEY.len() as u32,
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if len == 0 {
        return "[]";
    }
    let actual = len.min(buf.len() as u32) as usize;
    unsafe { core::str::from_utf8_unchecked(&buf[..actual]) }
}

fn write_storage(data: &[u8]) {
    unsafe {
        host_write_session_data(
            KEY.as_ptr(),
            KEY.len() as u32,
            data.as_ptr(),
            data.len() as u32,
        );
    }
}

// ── Tool implementations ────────────────────────────────────────────────────

fn cmd_add(args: &str, out: *mut u8, cap: u32) -> u32 {
    let text = json_get(args, "text").unwrap_or("task");

    let mut storage = [0u8; STORAGE_BUF];
    let existing = read_storage(&mut storage);
    let bytes = existing.as_bytes();

    // Find the current maximum ID.
    let mut max_id: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(id_str) = json_get(
            &bytes[i..]
                .first()
                .map(|_| unsafe { core::str::from_utf8_unchecked(&bytes[i..]) })
                .unwrap_or(""),
            "id",
        ) {
            if let Ok(n) = parse_u64(id_str) {
                if n > max_id {
                    max_id = n;
                }
            }
        }
        // Skip to next object
        if bytes[i] == b'{' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'{' {
                    depth += 1;
                } else if bytes[j] == b'}' {
                    depth -= 1;
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    let new_id = max_id + 1;
    let now = 1000000 + new_id;

    // Build new tasks list
    let mut buf = [0u8; STORAGE_BUF];
    let mut b = Buf(&mut buf, 0);

    // If existing is "[]", start fresh
    if existing.trim() == "[]" || existing.trim().is_empty() {
        b.s("[");
    } else {
        // Copy existing up to last ]
        let trim = existing.trim_end();
        let last = trim.rfind(']').unwrap_or(trim.len());
        b.s(&existing[..last]);
        if last > 1 {
            b.b(b',');
        }
    }
    b.b(b'{');
    b.s(r#""id":""#);
    b.num(new_id);
    b.s(r#"","text":"#);
    b.esc(text);
    b.s(r#","done":false,"created_at":"#);
    b.num(now);
    b.s(r#"}"#);
    b.b(b']');

    write_storage(b.as_slice());

    // Response
    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"id":""#);
    r.num(new_id);
    r.s(r#"","text":""#);
    r.s(text);
    r.s(r#"","status":"created"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_list(args: &str, out: *mut u8, cap: u32) -> u32 {
    let status = json_get(args, "status").unwrap_or("active");

    let mut storage = [0u8; STORAGE_BUF];
    let existing = read_storage(&mut storage);
    let bytes = existing.as_bytes();

    let mut buf = [0u8; STORAGE_BUF];
    let mut b = Buf(&mut buf, 0);
    b.b(b'[');

    let mut first = true;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                }
                i += 1;
            }
            let obj = &existing[start..i];
            let include = if status == "all" {
                true
            } else if status == "done" {
                json_get(obj, "done") == Some("true")
            } else {
                json_get(obj, "done") != Some("true")
            };
            if include {
                if !first {
                    b.b(b',');
                }
                first = false;
                for &ch in obj.as_bytes() {
                    b.b(ch);
                }
            }
        } else {
            i += 1;
        }
    }
    b.b(b']');

    copy_out(b.as_slice(), out, cap)
}

fn cmd_done(args: &str, out: *mut u8, cap: u32) -> u32 {
    let id = json_get(args, "id").unwrap_or("");

    let mut storage = [0u8; STORAGE_BUF];
    let existing = read_storage(&mut storage);
    let bytes = existing.as_bytes();

    // Find the task and replace "done":false with "done":true
    let mut buf = [0u8; STORAGE_BUF];
    let mut b = Buf(&mut buf, 0);
    let mut i = 0;
    let mut found = false;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                }
                i += 1;
            }
            let obj = &existing[start..i];
            let obj_id = json_get(obj, "id").unwrap_or("");
            if obj_id == id && !found {
                // Replace done:false → done:true in this object
                found = true;
                let done_str = r#""done":false"#;
                let replaced = r#""done":true"#;
                if let Some(pos) = obj.find(done_str) {
                    for &ch in obj[..pos].as_bytes() {
                        b.b(ch);
                    }
                    for &ch in replaced.as_bytes() {
                        b.b(ch);
                    }
                    for &ch in obj[pos + done_str.len()..].as_bytes() {
                        b.b(ch);
                    }
                } else {
                    for &ch in obj.as_bytes() {
                        b.b(ch);
                    }
                }
            } else {
                for &ch in obj.as_bytes() {
                    b.b(ch);
                }
            }
            // Add comma if not last
            let next = existing[start..]
                .find('}')
                .map(|p| start + p + 1)
                .unwrap_or(start);
            if next < bytes.len() && bytes[next] == b',' {
                b.b(b',');
                i += 1; // skip comma
            } else if i < bytes.len() && bytes[i] == b',' {
                b.b(b',');
                i += 1;
            }
        } else {
            b.b(bytes[i]);
            i += 1;
        }
    }

    if found {
        write_storage(b.as_slice());
    }

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"id":""#);
    r.s(id);
    r.s(r#"","status":"done"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_remove(args: &str, out: *mut u8, cap: u32) -> u32 {
    let id = json_get(args, "id").unwrap_or("");

    let mut storage = [0u8; STORAGE_BUF];
    let existing = read_storage(&mut storage);
    let bytes = existing.as_bytes();

    let mut buf = [0u8; STORAGE_BUF];
    let mut b = Buf(&mut buf, 0);
    b.b(b'[');
    let mut first = true;
    let mut i = 0;
    let mut removed = false;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                }
                i += 1;
            }
            let obj = &existing[start..i];
            let obj_id = json_get(obj, "id").unwrap_or("");
            if obj_id == id {
                removed = true;
            } else {
                if !first {
                    b.b(b',');
                }
                first = false;
                for &ch in obj.as_bytes() {
                    b.b(ch);
                }
            }
            // skip comma
            let next = existing[start..]
                .find('}')
                .map(|p| start + p + 1)
                .unwrap_or(start);
            if next < bytes.len() && bytes[next] == b',' {
                i += 1;
            } else if i < bytes.len() && bytes[i] == b',' {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    b.b(b']');
    if removed {
        write_storage(b.as_slice());
    }

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"id":""#);
    r.s(id);
    r.s(r#"","status":"removed"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_clean(_args: &str, out: *mut u8, cap: u32) -> u32 {
    let mut storage = [0u8; STORAGE_BUF];
    let existing = read_storage(&mut storage);
    let bytes = existing.as_bytes();

    let mut buf = [0u8; STORAGE_BUF];
    let mut b = Buf(&mut buf, 0);
    b.b(b'[');
    let mut first = true;
    let mut removed: u64 = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'{' {
                    depth += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                }
                i += 1;
            }
            let obj = &existing[start..i];
            let is_done = json_get(obj, "done") == Some("true");
            if is_done {
                removed += 1;
            } else {
                if !first {
                    b.b(b',');
                }
                first = false;
                for &ch in obj.as_bytes() {
                    b.b(ch);
                }
            }
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    b.b(b']');
    write_storage(b.as_slice());

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"removed":"#);
    r.num(removed);
    r.s(r#","status":"done"}"#);
    copy_out(r.as_slice(), out, cap)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn parse_u64(s: &str) -> Result<u64, ()> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(());
    }
    let mut n: u64 = 0;
    for &b in bytes {
        if b < b'0' || b > b'9' {
            return Err(());
        }
        n = n.wrapping_mul(10).wrapping_add((b - b'0') as u64);
    }
    Ok(n)
}

// ── Unit tests ───────────────────────────────────────────────────────────────
//
// These run under the native target (`cargo test -p todo-extension`) and
// exercise the pure-logic helpers (parse_u64, json_get, json_skip_objects)
// WITHOUT touching host imports. The host-side integration test that loads
// the compiled WASM lives in `tests/host_integration.rs`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u64_basic() {
        assert_eq!(parse_u64("0"), Ok(0));
        assert_eq!(parse_u64("1"), Ok(1));
        assert_eq!(parse_u64("42"), Ok(42));
        assert_eq!(parse_u64("99999"), Ok(99_999));
    }

    #[test]
    fn parse_u64_rejects_non_digits() {
        assert_eq!(parse_u64(""), Err(()));
        assert_eq!(parse_u64("abc"), Err(()));
        assert_eq!(parse_u64("12a"), Err(()));
        assert_eq!(parse_u64("1.5"), Err(()));
        assert_eq!(parse_u64("-1"), Err(()));
    }

    #[test]
    fn json_get_string_value() {
        let json = r#"{"text":"hello"}"#;
        assert_eq!(json_get(json, "text"), Some("hello"));
    }

    #[test]
    fn json_get_numeric_value() {
        let json = r#"{"id":5}"#;
        assert_eq!(json_get(json, "id"), Some("5"));
    }

    #[test]
    fn json_get_boolean_value() {
        let json = r#"{"done":true}"#;
        assert_eq!(json_get(json, "done"), Some("true"));
    }

    #[test]
    fn json_get_skips_earlier_keys() {
        let json = r#"{"a":"first","b":"second"}"#;
        assert_eq!(json_get(json, "a"), Some("first"));
        assert_eq!(json_get(json, "b"), Some("second"));
    }

    #[test]
    fn json_get_missing_key_returns_none() {
        let json = r#"{"a":"first"}"#;
        assert_eq!(json_get(json, "missing"), None);
    }

    #[test]
    fn json_skip_objects_counts_correctly() {
        // Three top-level objects.
        let json = r#"[{"id":1},{"id":2},{"id":3}]"#;
        // Skipping 0 objects returns the end of the first object.
        let p0 = json_skip_objects(json, 0);
        assert!(json[..p0].ends_with('}'));
        // Skipping past all objects returns the end of the string.
        let p_all = json_skip_objects(json, 100);
        assert_eq!(p_all, json.len());
    }
}
