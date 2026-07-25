//! Plan Extension -- manages a single plan file on disk.
//!
//! A plan is a plain text file: one step per line. Pending steps have no
//! prefix; completed steps are prefixed with `[x] `.
//!
//! Tools:
//!   plan_enter({plan_path}) -> remember path, create file if missing
//!   plan_exit()             -> clear the remembered path
//!   plan_add({step})        -> append a step line
//!   plan_list()             -> read all steps
//!   plan_done({index})      -> prefix line at index with `[x] `
//!
//! Build:
//!   cargo build --target wasm32-wasip1 --release -p plan-extension
//!   cp target/wasm32-wasip1/release/plan_extension.wasm <project>/.ion/extensions/

#![no_std]

// ── Host functions (provided by the ION worker) ─────────────────────────────

extern "C" {
    // Tool registration.
    fn host_register_tool(
        name_ptr: *const u8, name_len: u32,
        desc_ptr: *const u8, desc_len: u32,
        schema_ptr: *const u8, schema_len: u32,
    );
    // Read a file into the provided buffer; returns the number of bytes read.
    fn host_read_file(
        path_ptr: *const u8, path_len: u32,
        out_buf: *mut u8, out_capacity: u32,
    ) -> u32;
    // Write bytes to a file (overwrite). Returns 0 on success.
    fn host_write_file(
        path_ptr: *const u8, path_len: u32,
        content_ptr: *const u8, content_len: u32,
    ) -> u32;
    // Return 1 if the path exists, 0 otherwise.
    fn host_path_exists(path_ptr: *const u8, path_len: u32) -> u32;
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

/// Extract a string field value from a JSON object (simplified; no nesting).
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
                && (bytes[v] == b':'
                    || bytes[v] == b' '
                    || bytes[v] == b'\t')
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
    fn as_slice(&self) -> &[u8] {
        &self.0[..self.1]
    }
    fn len(&self) -> usize {
        self.1
    }
}

// ── Static plan path buffer ─────────────────────────────────────────────────
//
// WASM is single-threaded, so a mutable static is safe here. The remembered
// path is set by plan_enter and cleared by plan_exit.

static mut PLAN_PATH: [u8; 512] = [0; 512];
static mut PLAN_PATH_LEN: usize = 0;

fn set_plan_path(path: &[u8]) {
    unsafe {
        let cap = PLAN_PATH.len();
        let n = path.len().min(cap);
        PLAN_PATH[..n].copy_from_slice(&path[..n]);
        PLAN_PATH_LEN = n;
    }
}

fn clear_plan_path() {
    unsafe {
        PLAN_PATH_LEN = 0;
    }
}

/// Return the current remembered path as a &str slice, or None if cleared.
fn current_path() -> Option<&'static str> {
    unsafe {
        let len = PLAN_PATH_LEN;
        if len == 0 {
            return None;
        }
        // SAFETY: bytes were written by set_plan_path from a UTF-8 string.
        Some(core::str::from_utf8_unchecked(&PLAN_PATH[..len]))
    }
}

// ── File I/O wrappers ───────────────────────────────────────────────────────

const FILE_BUF: usize = 16384;
const OUT_BUF: usize = 4096;

fn read_plan_file(buf: &mut [u8]) -> &str {
    let path = match current_path() {
        Some(p) => p,
        None => return "",
    };
    let len = unsafe {
        host_read_file(
            path.as_ptr(),
            path.len() as u32,
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if len == 0 {
        return "";
    }
    let actual = (len as usize).min(buf.len());
    // SAFETY: host writes plain bytes; treat as UTF-8 (plan lines are text).
    unsafe { core::str::from_utf8_unchecked(&buf[..actual]) }
}

fn write_plan_file(content: &[u8]) -> u32 {
    let path = match current_path() {
        Some(p) => p,
        None => return 1,
    };
    unsafe { host_write_file(path.as_ptr(), path.len() as u32, content.as_ptr(), content.len() as u32) }
}

fn path_exists(path: &str) -> bool {
    let r = unsafe { host_path_exists(path.as_ptr(), path.len() as u32) };
    r != 0
}

fn copy_out(src: &[u8], out: *mut u8, cap: u32) -> u32 {
    let len = src.len().min(cap as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), out, len);
    }
    len as u32
}

// ── Extension entry points ──────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn extension_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn extension_init() {
    host_register(
        "plan_enter",
        "Enter planning mode and remember the plan file path. \
         If the file does not exist, it is created empty.\n\
         Args: {plan_path: string}.\n\
         Returns: {status, plan_path, mode}",
        r#"{"type":"object","properties":{"plan_path":{"type":"string"}},"required":["plan_path"]}"#,
    );
    host_register(
        "plan_exit",
        "Exit planning mode and clear the remembered path.\n\
         Args: {}.\n\
         Returns: {status, mode}",
        r#"{"type":"object","properties":{}}"#,
    );
    host_register(
        "plan_add",
        "Append a step line to the plan file.\n\
         Args: {step: string}.\n\
         Returns: {status, step, index}",
        r#"{"type":"object","properties":{"step":{"type":"string"}},"required":["step"]}"#,
    );
    host_register(
        "plan_list",
        "List all steps in the plan file.\n\
         Args: {}.\n\
         Returns: {steps:[...], count}",
        r#"{"type":"object","properties":{}}"#,
    );
    host_register(
        "plan_done",
        "Mark the step at the given 0-based index as done (prefix `[x] `).\n\
         Args: {index: number}.\n\
         Returns: {status, index}",
        r#"{"type":"object","properties":{"index":{"type":"number"}},"required":["index"]}"#,
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
        "plan_enter" => cmd_enter(args, out_buf, out_capacity),
        "plan_exit" => cmd_exit(args, out_buf, out_capacity),
        "plan_add" => cmd_add(args, out_buf, out_capacity),
        "plan_list" => cmd_list(args, out_buf, out_capacity),
        "plan_done" => cmd_done(args, out_buf, out_capacity),
        _ => {
            let e = b"unknown tool";
            copy_out(e, out_buf, out_capacity)
        }
    }
}

// ── Tool implementations ────────────────────────────────────────────────────

fn cmd_enter(args: &str, out: *mut u8, cap: u32) -> u32 {
    let path = json_get(args, "plan_path").unwrap_or("");
    set_plan_path(path.as_bytes());

    // Create the file if it does not exist.
    if !path.is_empty() && !path_exists(path) {
        // Writing an empty buffer creates the file.
        write_plan_file(b"");
    }

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"status":"ok","plan_path":"#);
    r.esc(path);
    r.s(r#","mode":"plan"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_exit(_args: &str, out: *mut u8, cap: u32) -> u32 {
    clear_plan_path();

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"status":"ok","mode":"normal"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_add(args: &str, out: *mut u8, cap: u32) -> u32 {
    let step = json_get(args, "step").unwrap_or("");

    let mut file_buf = [0u8; FILE_BUF];
    let existing = read_plan_file(&mut file_buf);

    // Count existing lines to compute the new 0-based index.
    let index = count_lines(existing);

    // Build new content: existing + "\n" + step + trailing "\n".
    // Use Buf's own as_slice() to inspect the last byte (avoids indexing
    // content while Buf holds a mutable borrow — E0503).
    let mut content = [0u8; FILE_BUF];
    let mut c = Buf(&mut content, 0);
    let needs_nl = !existing.is_empty() && !existing.ends_with('\n');
    for &b in existing.as_bytes() {
        c.b(b);
    }
    if needs_nl {
        c.b(b'\n');
    }
    for &b in step.as_bytes() {
        c.b(b);
    }
    let needs_trailing_nl = {
        let s = c.as_slice();
        s.last() != Some(&b'\n')
    };
    if needs_trailing_nl {
        c.b(b'\n');
    }
    let final_len = c.len();
    drop(c);

    // Write the full content (including trailing newline) and CHECK the result.
    // host_write_file returns: 0=success, 1=no fs, 2=traversal blocked, 3=IO error.
    let write_result = write_plan_file(&content[..final_len]);

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    if write_result == 0 {
        r.s(r#"{"status":"added","step":"#);
        r.esc(step);
        r.s(r#","index":"#);
        r.num(index as u64);
        r.s(r#"}"#);
    } else {
        // Encode the failure reason so callers can diagnose.
        r.s(r#"{"status":"error","code":"#);
        r.num(write_result as u64);
        r.s(r#","reason":""#);
        let reason = match write_result {
            1 => "no fs capability injected",
            2 => "path traversal blocked (outside allowed_roots)",
            3 => "IO error",
            _ => "unknown",
        };
        r.s(reason);
        r.s(r#""}"#);
    }
    copy_out(r.as_slice(), out, cap)
}

fn cmd_list(_args: &str, out: *mut u8, cap: u32) -> u32 {
    let mut file_buf = [0u8; FILE_BUF];
    let existing = read_plan_file(&mut file_buf);

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"steps":["#);

    let mut count: u64 = 0;
    for line in iter_lines(existing) {
        if count > 0 {
            r.s(r#","#);
        }
        r.esc(line);
        count += 1;
    }
    r.s(r#"],"count":"#);
    r.num(count);
    r.s(r#"}"#);
    copy_out(r.as_slice(), out, cap)
}

fn cmd_done(args: &str, out: *mut u8, cap: u32) -> u32 {
    let index_str = json_get(args, "index").unwrap_or("0");
    let index: usize = parse_usize(index_str);

    let mut file_buf = [0u8; FILE_BUF];
    let existing = read_plan_file(&mut file_buf);

    let mut content = [0u8; FILE_BUF];
    let mut c = Buf(&mut content, 0);
    let mut current: usize = 0;
    let mut found = false;

    for line in iter_lines(existing) {
        if current == index {
            found = true;
            // Prefix with `[x] ` if not already marked.
            if line.starts_with("[x] ") {
                c.s(line);
            } else {
                c.s("[x] ");
                c.s(line);
            }
        } else {
            c.s(line);
        }
        c.b(b'\n');
        current += 1;
    }

    if found {
        write_plan_file(c.as_slice());
    }

    let mut resp = [0u8; OUT_BUF];
    let mut r = Buf(&mut resp, 0);
    r.s(r#"{"status":"#);
    if found {
        r.s(r#""done""#);
    } else {
        r.s(r#""not_found""#);
    }
    r.s(r#","index":"#);
    r.num(index as u64);
    r.s(r#"}"#);
    copy_out(r.as_slice(), out, cap)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Count the number of newline-terminated lines in `s`.
fn count_lines(s: &str) -> usize {
    let mut n = 0;
    for &b in s.as_bytes() {
        if b == b'\n' {
            n += 1;
        }
    }
    n
}

/// Iterate over lines (excluding trailing newlines). The final unterminated
/// line is also yielded if it is non-empty.
fn iter_lines(s: &str) -> impl Iterator<Item = &str> {
    LineIter {
        bytes: s.as_bytes(),
        pos: 0,
    }
}

struct LineIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for LineIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.pos >= self.bytes.len() {
            return None;
        }
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
        let line = unsafe { core::str::from_utf8_unchecked(&self.bytes[start..self.pos]) };
        if self.pos < self.bytes.len() {
            // Skip the newline.
            self.pos += 1;
        }
        if line.is_empty() {
            // Skip blank lines but keep the iteration going.
            return self.next();
        }
        Some(line)
    }
}

fn parse_usize(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut n: usize = 0;
    for &b in bytes {
        if b >= b'0' && b <= b'9' {
            n = n.wrapping_mul(10).wrapping_add((b - b'0') as usize);
        } else {
            break;
        }
    }
    n
}

// ── Unit tests ───────────────────────────────────────────────────────────────
//
// These run under the native target (`cargo test -p plan-extension`) and
// exercise the pure-logic helpers (parse_usize, count_lines, iter_lines,
// json_get) WITHOUT touching host imports. The host-side integration test
// that loads the compiled WASM lives in `tests/host_integration.rs`.

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::prelude::rust_2021::*;
    use std::vec::Vec;

    #[test]
    fn parse_usize_basic() {
        assert_eq!(parse_usize("0"), 0);
        assert_eq!(parse_usize("1"), 1);
        assert_eq!(parse_usize("42"), 42);
        assert_eq!(parse_usize("99999"), 99_999);
    }

    #[test]
    fn parse_usize_stops_on_non_digit() {
        assert_eq!(parse_usize("12a"), 12);
        assert_eq!(parse_usize(""), 0);
        assert_eq!(parse_usize("abc"), 0);
    }

    #[test]
    fn count_lines_counts_newlines() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb\n"), 2);
        assert_eq!(count_lines("a\nb\nc\n"), 3);
    }

    #[test]
    fn iter_lines_yields_each_step() {
        let s = "step one\nstep two\nstep three\n";
        let lines: Vec<&str> = iter_lines(s).collect();
        assert_eq!(lines, vec!["step one", "step two", "step three"]);
    }

    #[test]
    fn iter_lines_skips_blank_lines() {
        let s = "a\n\nb\n";
        let lines: Vec<&str> = iter_lines(s).collect();
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn iter_lines_handles_trailing_unterminated() {
        let s = "a\nb";
        let lines: Vec<&str> = iter_lines(s).collect();
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn json_get_string_value() {
        let json = r#"{"plan_path":"/tmp/plan.md"}"#;
        assert_eq!(json_get(json, "plan_path"), Some("/tmp/plan.md"));
    }

    #[test]
    fn json_get_numeric_value() {
        let json = r#"{"index":2}"#;
        assert_eq!(json_get(json, "index"), Some("2"));
    }

    #[test]
    fn json_get_step_value() {
        let json = r#"{"step":"write tests"}"#;
        assert_eq!(json_get(json, "step"), Some("write tests"));
    }
}
