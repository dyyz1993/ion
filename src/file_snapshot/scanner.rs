//! 目录扫描（路线 2：bash 兜底 + turn_end 兜底）
//!
//! 快速扫描 cwd 目录树，用 mtime+size 识别变化文件。
//! 独立忽略清单过滤（文件夹跳过 / 文件查二进制）。
//!
//! **关键原则：不遵守 .gitignore。** agent 可能改了 .gitignore 忽略的文件
//! （.env、本地配置等），这些改动同样需要被追踪和回滚。
//! 只用内置 DEFAULT_IGNORE 清单跳过体积大、可重建的产物目录。

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

    // 只用内置 DEFAULT_IGNORE，不读 .gitignore（见模块注释）
    let ignore_patterns: Vec<String> = DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect();
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

/// 内置忽略清单：体积大、可重建的产物目录与二进制文件。
/// **不读 .gitignore** —— agent 改的 .env / 本地配置等同样需要追踪。
const DEFAULT_IGNORE: &[&str] = &[
    // VCS
    ".git",
    // 语言生态产物目录
    "node_modules", "target", "__pycache__", ".cache",
    ".venv", "venv", "build", "dist", "out",
    ".next", ".nuxt", ".gradle", ".m2", "Pods",
    // 编译产物（按扩展名）
    "*.pyc", "*.o", "*.so", "*.dylib", "*.dll", "*.a",
    // 图片（二进制，diff 无意义）
    "*.png", "*.jpg", "*.jpeg", "*.gif", "*.webp", "*.ico",
    // 日志/锁/交换文件
    "*.lock", "*.log", "*.swp",
    // 压缩包
    "*.zip", "*.tar", "*.gz", "*.bz2", "*.7z",
    // WASM 二进制
    "*.wasm",
];

/// 检查路径是否被忽略（仅按 DEFAULT_IGNORE，不读 .gitignore）
/// 规则：命中清单的文件夹 → 跳过；命中清单的文件 → 查二进制，文本保留
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

    #[test]
    fn scan_does_not_respect_gitignore() {
        // 红线测试：.gitignore 里写的文本文件仍应被扫描
        let tmp = std::env::temp_dir().join(format!("fs_scan_gi_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        // 写一个 .gitignore，忽略 .env 和 local.conf
        std::fs::write(tmp.join(".gitignore"), ".env\nlocal.conf\n").unwrap();
        // 这两个文件是文本，即使 .gitignore 忽略也应被扫到
        std::fs::write(tmp.join(".env"), "SECRET=hello").unwrap();
        std::fs::write(tmp.join("local.conf"), "debug=true").unwrap();
        // target/ 在 DEFAULT_IGNORE 里，应被跳过
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("target/out"), "x").unwrap();

        let result = scan_dir_fast(tmp.to_string_lossy().as_ref());

        // .gitignore 忽略的文本文件 —— 仍被扫到（不遵守 .gitignore）
        assert!(result.files.contains_key(".env"), ".env 应被扫到（不遵守 gitignore）");
        assert!(result.files.contains_key("local.conf"), "local.conf 应被扫到");
        // DEFAULT_IGNORE 里的目录 —— 被跳过
        assert!(!result.files.contains_key("target/out"), "target/ 应被忽略");
        // .gitignore 文件本身也该被扫到（它是文本文件）
        assert!(result.files.contains_key(".gitignore"), ".gitignore 应被扫到");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
