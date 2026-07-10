//! 目录扫描（路线 2：bash 兜底）
//!
//! 快速扫描 cwd 目录树，用 mtime+size 识别变化文件。
//! git ignore 智能过滤：文件夹跳过 / 文件查二进制。

use std::collections::HashMap;
use std::path::Path;

/// 快速扫描结果：path → (mtime, size)，不读内容
pub struct DirScanResult {
    pub files: HashMap<String, (std::time::SystemTime, u64)>,
    pub truncated: bool,
}

/// 扫描限制
pub const MAX_FILE_SIZE: u64 = 1 * 1024 * 1024; // 1MB
pub const MAX_TOTAL_SIZE: u64 = 50 * 1024 * 1024; // 50MB
pub const MAX_FILE_COUNT: usize = 5000;
pub const MAX_DEPTH: usize = 10;

/// 快速扫描目录（只 stat，不读内容）
pub fn scan_dir_fast(cwd: &str) -> DirScanResult {
    let mut files = HashMap::new();
    let mut truncated = false;
    let mut total_size = 0u64;

    let ignore_patterns = load_gitignore(cwd);
    scan_recursive(
        Path::new(cwd),
        Path::new(cwd),
        &ignore_patterns,
        &mut files,
        &mut total_size,
        &mut truncated,
        0,
    );

    DirScanResult { files, truncated }
}

fn scan_recursive(
    dir: &Path,
    root: &Path,
    ignore: &[String],
    files: &mut HashMap<String, (std::time::SystemTime, u64)>,
    total_size: &mut u64,
    truncated: &mut bool,
    depth: usize,
) {
    if *truncated || depth > MAX_DEPTH {
        return;
    }
    if files.len() > MAX_FILE_COUNT {
        *truncated = true;
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if *truncated || files.len() > MAX_FILE_COUNT {
            *truncated = true;
            return;
        }

        let path = entry.path();
        let rel_path = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // git ignore 检查
        if is_ignored(&rel_path, &path, ignore) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            scan_recursive(&path, root, ignore, files, total_size, truncated, depth + 1);
        } else if metadata.is_file() {
            let size = metadata.len();
            if size > MAX_FILE_SIZE {
                continue; // 大文件跳过
            }
            *total_size += size;
            if *total_size > MAX_TOTAL_SIZE {
                *truncated = true;
                return;
            }
            let mtime = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.insert(rel_path, (mtime, size));
        }
    }
}

/// 加载 .gitignore
fn load_gitignore(cwd: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    // 默认忽略
    patterns.extend(DEFAULT_IGNORE.iter().map(|s| s.to_string()));
    // 项目的 .gitignore
    let gitignore_path = Path::new(cwd).join(".gitignore");
    if let Ok(content) = std::fs::read_to_string(&gitignore_path) {
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                patterns.push(line.to_string());
            }
        }
    }
    patterns
}

const DEFAULT_IGNORE: &[&str] = &[
    ".git", "node_modules", "target", "__pycache__", ".cache",
    "*.pyc", "*.o", "*.so", "*.dylib", "*.dll",
    "*.png", "*.jpg", "*.jpeg", "*.gif", "*.webp", "*.ico",
    "*.zip", "*.tar", "*.gz", "*.bz2", "*.7z",
    "*.wasm", "*.dylib",
];

/// 检查路径是否被忽略
/// 规则：git ignore 文件夹 → 跳过；git ignore 文件 → 查二进制
fn is_ignored(rel_path: &str, full_path: &Path, patterns: &[String]) -> bool {
    for pattern in patterns {
        if matches_pattern(rel_path, pattern) {
            // 命中 ignore，检查是不是文件夹
            if full_path.is_dir() {
                return true; // 文件夹 → 跳过
            }
            // 文件 → 检查是否二进制
            if is_binary(full_path) {
                return true; // 二进制 → 跳过
            }
            // 文本文件（.env 等）→ 不跳过，保留
            return false;
        }
    }
    false
}

/// 简单 glob 匹配
fn matches_pattern(path: &str, pattern: &str) -> bool {
    // 去掉 trailing /
    let pat = pattern.trim_end_matches('/');
    // 直接匹配
    if path == pat || path.starts_with(&format!("{}/", pat)) {
        return true;
    }
    // 后缀匹配 (*.log → xxx.log)
    if let Some(ext) = pat.strip_prefix("*.") {
        return path.ends_with(&format!(".{}", ext));
    }
    // 包含匹配（path 的某段等于 pat）
    path.split('/').any(|seg| seg == pat)
}

/// 二进制检测：读前 8KB，含 null byte 或不可打印字符占比 > 30% 则判定二进制
pub fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return true, // 读不了就当二进制（跳过）
    };
    let mut buf = [0u8; 8192];
    let n = match file.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return false, // 空文件当文本
    };
    let data = &buf[..n];
    // 含 null byte → 二进制
    if data.contains(&0u8) {
        return true;
    }
    // 不可打印字符占比 > 30%
    let non_printable = data.iter()
        .filter(|&&b| b < 0x09 || (b > 0x0D && b < 0x20))
        .count();
    (non_printable as f64 / n as f64) > 0.30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_basic_dir() {
        let tmp = std::env::temp_dir().join(format!("fs_scan_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), "hello").unwrap();
        std::fs::write(tmp.join("b.rs"), "fn main(){}").unwrap();

        let result = scan_dir_fast(tmp.to_string_lossy().as_ref());
        assert!(result.files.contains_key("a.txt"));
        assert!(result.files.contains_key("b.rs"));
        assert!(!result.truncated);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn scan_ignores_target_dir() {
        let tmp = std::env::temp_dir().join(format!("fs_scan_ig_{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("target/out"), "x").unwrap();
        std::fs::write(tmp.join("main.rs"), "fn main(){}").unwrap();

        let result = scan_dir_fast(tmp.to_string_lossy().as_ref());
        assert!(result.files.contains_key("main.rs"));
        assert!(!result.files.contains_key("target/out"), "target/ 应被忽略");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn is_binary_detection() {
        let tmp = std::env::temp_dir().join(format!("fs_bin_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let text_file = tmp.join("text.txt");
        std::fs::write(&text_file, "hello world\n这是文本").unwrap();
        assert!(!is_binary(&text_file), "文本文件不应被判为二进制");

        let bin_file = tmp.join("data.bin");
        std::fs::write(&bin_file, [0u8, 1, 0, 255, 0, 2]).unwrap();
        assert!(is_binary(&bin_file), "含 null byte 应判为二进制");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
