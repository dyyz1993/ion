//! Content-addressed object store — 文件内容按 hash 去重存储
//!
//! 设计：类似 mini-git 的 object store
//! - 内容 → SipHash → 路径 objects/<前2位>/<剩余hash>
//! - 相同内容只存一次（exists 判断）
//! - 内容用 zstd 压缩存储（hash 算原始内容，压缩在 hash 之后）
//! - 旧明文数据通过 magic bytes 检测自动兼容（read 时区分压缩/明文）
//!
//! 注：当前用 std DefaultHasher（SipHash-1-3，64bit）。
//! 对小项目够用；文件量到百万级时有碰撞风险，届时升级 sha256。

use std::path::{Path, PathBuf};

/// zstd 压缩数据的 magic number（前 4 字节）
/// 用于 read_object 区分压缩数据（新写入）和明文数据（旧数据兼容）
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// zstd 压缩级别（1-22，默认 3 = 速度/压缩比平衡）
const ZSTD_LEVEL: i32 = 3;

/// 小于此字节数的文件不压缩（zstd 头开销 ~10-15 bytes，小文件压缩反而变大）
/// 阈值 64：实测 45 bytes 文件压缩后变 124%，64 bytes 以上压缩开始有收益
const MIN_COMPRESS_SIZE: usize = 64;

/// Content-addressed object store
pub struct ObjectStore {
    /// 存储根目录（~/.ion/file-store/<project_key>/）
    store_dir: PathBuf,
}

/// 写入 object 的结果
pub struct WriteResult {
    pub hash: String,
    pub deduped: bool, // true = 已存在，跳过写入
}

impl ObjectStore {
    /// 为指定项目创建 ObjectStore
    pub fn for_project(project_key: &str) -> Self {
        let store_dir = crate::paths::file_store_dir(project_key);
        std::fs::create_dir_all(store_dir.join("objects")).ok();
        Self { store_dir }
    }

    /// 从 cwd 创建（自动算 project_key）
    pub fn for_cwd(cwd: &str) -> Self {
        Self::for_project(&project_key(cwd))
    }

    /// 直接指定存储目录（测试用）
    #[cfg(test)]
    pub fn new_at(store_dir: PathBuf) -> Self {
        std::fs::create_dir_all(store_dir.join("objects")).ok();
        Self { store_dir }
    }

    /// 存储根目录
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    /// 写入内容，返回 hash + 去重标记
    /// hash 算原始内容（保证去重逻辑不变），压缩后写盘
    pub fn write_object(&self, content: &[u8]) -> WriteResult {
        let hash = content_hash(content);
        let path = self.object_path(&hash);

        if path.exists() {
            // 去重：已存在，直接返回
            return WriteResult {
                hash,
                deduped: true,
            };
        }

        // zstd 压缩后写盘（降低存储，hash 仍基于原始内容）
        // 小文件（< MIN_COMPRESS_SIZE）不压缩，因为 zstd 头开销会让压缩后更大
        let to_write: Vec<u8> = if content.len() >= MIN_COMPRESS_SIZE {
            match zstd::encode_all(content, ZSTD_LEVEL) {
                Ok(c) => c,
                Err(_) => content.to_vec(), // 压缩失败 → 回退明文
            }
        } else {
            content.to_vec() // 小文件 → 明文
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, &to_write).ok();

        WriteResult {
            hash,
            deduped: false,
        }
    }

    /// 读取内容
    /// 自动检测压缩（zstd magic bytes）和明文格式，兼容旧数据
    pub fn read_object(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.object_path(hash);
        let bytes = std::fs::read(&path).ok()?;

        // 检测 zstd 压缩（magic bytes）
        if bytes.starts_with(&ZSTD_MAGIC) {
            if let Ok(decompressed) = zstd::decode_all(&bytes[..]) {
                return Some(decompressed);
            }
            // 解压失败 → 可能是碰巧以 magic 开头的明文 → fallthrough 到明文返回
        }
        Some(bytes)
    }

    /// 读取为 UTF-8 字符串（diff 用）
    pub fn read_object_text(&self, hash: &str) -> Option<String> {
        String::from_utf8(self.read_object(hash)?).ok()
    }

    /// object 是否存在
    pub fn exists(&self, hash: &str) -> bool {
        self.object_path(hash).exists()
    }

    /// object 文件路径（pub：GC 需要拿 object 路径算 mtime）
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = if hash.len() >= 2 {
            hash.split_at(2)
        } else {
            (hash, "")
        };
        self.store_dir.join("objects").join(prefix).join(rest)
    }

    /// 获取存储总大小（bytes）
    pub fn store_size(&self) -> u64 {
        let objects_dir = self.store_dir.join("objects");
        dir_size(&objects_dir)
    }

    /// Return the number of objects in the object store
    /// Traverse all subdirectories in the objects dir, count files
    pub fn store_count(&self) -> Result<usize, String> {
        let objects_dir = self.store_dir.join("objects");
        if !objects_dir.exists() {
            return Ok(0);
        }
        let mut count = 0usize;
        let prefix_dirs = std::fs::read_dir(&objects_dir)
            .map_err(|e| format!("Failed to read objects dir: {}", e))?;
        for prefix_entry in prefix_dirs {
            let prefix_entry = prefix_entry
                .map_err(|e| format!("Failed to read dir entry: {}", e))?;
            let path = prefix_entry.path();
            if path.is_dir() {
                if let Ok(files) = std::fs::read_dir(&path) {
                    for file in files {
                        if file.is_ok() {
                            count += 1;
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    /// 列出所有 object hash（GC 用）
    pub fn list_objects(&self) -> Vec<String> {
        let objects_dir = self.store_dir.join("objects");
        let mut hashes = Vec::new();
        if let Ok(prefix_dirs) = std::fs::read_dir(&objects_dir) {
            for prefix_entry in prefix_dirs.flatten() {
                let prefix = prefix_entry.file_name().to_string_lossy().to_string();
                if let Ok(files) = std::fs::read_dir(prefix_entry.path()) {
                    for file in files.flatten() {
                        let rest = file.file_name().to_string_lossy().to_string();
                        hashes.push(format!("{}{}", prefix, rest));
                    }
                }
            }
        }
        hashes
    }

    /// 删除 object（GC 用）
    pub fn delete_object(&self, hash: &str) {
        let _ = std::fs::remove_file(self.object_path(hash));
    }
}

/// 递归计算目录大小
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// 计算内容的 SipHash hash（64bit，16 位 hex 字符串）
pub fn content_hash(content: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 计算 project key
/// git 仓库：用 git-common-dir（主仓库和 worktree 一致）
/// 非 git：fallback 到 cwd 的 hash
///
/// 委托给 `paths::project_key_git`（缺口 #3：统一 project_key 体系）
pub fn project_key(cwd: &str) -> String {
    crate::paths::project_key_git(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_object() {
        let tmp = std::env::temp_dir().join(format!("fs_obj_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // 写入
        let result = store.write_object(b"hello world");
        assert!(!result.deduped, "首次写入不应去重");
        let hash1 = result.hash.clone();

        // 再写相同内容 → 去重
        let result2 = store.write_object(b"hello world");
        assert!(result2.deduped, "相同内容应去重");
        assert_eq!(result2.hash, hash1);

        // 读回
        let content = store.read_object(&hash1).unwrap();
        assert_eq!(content, b"hello world");

        // 不同内容 → 不同 hash
        let result3 = store.write_object(b"hello world!");
        assert_ne!(result3.hash, hash1);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn content_hash_deterministic() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }

    #[test]
    fn project_key_git_repo() {
        // 当前目录是 git 仓库
        let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        let key = project_key(&cwd);
        assert!(!key.is_empty());
        // 应该是 16 位 hex
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn project_key_worktree_shares_with_main() {
        // 验证 project_key 在 worktree 和主仓库下一致
        let main_cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        let main_key = project_key(&main_cwd);

        // 造一个临时 worktree
        let wt_path = format!("/tmp/ion_wt_pk_test_{}", std::process::id());
        let output = std::process::Command::new("git")
            .args(["worktree", "add", &wt_path])
            .current_dir(&main_cwd)
            .output();

        if output.is_ok() && output.as_ref().unwrap().status.success() {
            let wt_key = project_key(&wt_path);
            // worktree 的 project_key 应与主仓库一致
            assert_eq!(main_key, wt_key,
                "project_key 应共享: main={main_key} wt={wt_key}");

            // 清理 worktree
            let _ = std::process::Command::new("git")
                .args(["worktree", "remove", &wt_path, "--force"])
                .current_dir(&main_cwd)
                .output();
        }
        // 如果 git worktree add 失败（如 CI 环境无 git），跳过
    }

    #[test]
    fn project_key_non_git_fallback() {
        // 非 git 目录 fallback 到 cwd hash
        let tmp = std::env::temp_dir().join(format!("fs_nongit_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_string_lossy().to_string();
        let key = project_key(&cwd);
        assert!(!key.is_empty());
        assert_eq!(key.len(), 16);
        // 非 git 目录的 key 应不同于 git 仓库的 key
        let git_cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
        let git_key = project_key(&git_cwd);
        assert_ne!(key, git_key, "非 git 目录 key 应不同于 git 仓库");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn object_store_shares_between_worktrees() {
        // 验证两个 worktree 的 ObjectStore 写入相同内容 → 同一 object
        let main_cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();

        // 用 project_key 创建 store（两个 worktree 用同一个 key → 同一目录）
        let key = project_key(&main_cwd);
        let store1 = ObjectStore::for_project(&key);
        let store2 = ObjectStore::for_project(&key);

        // store1 写入
        let r1 = store1.write_object(b"shared content");
        // store2 读 → 应能读到（共享存储）
        let content = store2.read_object(&r1.hash);
        assert!(content.is_some(), "worktree 间应共享 object store");
        assert_eq!(content.unwrap(), b"shared content");

        // store2 再写相同内容 → 去重
        let r2 = store2.write_object(b"shared content");
        assert!(r2.deduped, "跨 worktree 相同内容应去重");
        assert_eq!(r1.hash, r2.hash);
    }

    #[test]
    fn compression_roundtrip_large_text() {
        // 大文本压缩 → 读回内容一致
        let tmp = std::env::temp_dir().join(format!("fs_zstd_rt_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        let big = "a".repeat(10000);
        let r = store.write_object(big.as_bytes());
        assert!(!r.deduped, "首次写入不应去重");

        let read = store.read_object(&r.hash).unwrap();
        assert_eq!(read, big.as_bytes(), "压缩后读回应与原始一致");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn small_file_not_compressed() {
        // 小文件（< MIN_COMPRESS_SIZE）不压缩，避免 zstd 头开销导致变大
        let tmp = std::env::temp_dir().join(format!("fs_zstd_small_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        let small = b"tiny"; // 4 bytes，远小于 MIN_COMPRESS_SIZE(64)
        let r = store.write_object(small);

        // 验证存储的是明文（不是 zstd 压缩格式）
        let path = store.object_path(&r.hash);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, small, "小文件应明文存储（无压缩）");
        assert!(!on_disk.starts_with(&ZSTD_MAGIC), "小文件不应有 zstd magic");

        // 读回正确
        let read = store.read_object(&r.hash).unwrap();
        assert_eq!(read, small);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reads_uncompressed_legacy_data() {
        // 模拟旧明文数据：手写明文 object → read_object 应能读（magic bytes 兼容）
        let tmp = std::env::temp_dir().join(format!("fs_zstd_legacy_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // 手写明文（绕过 write_object 的压缩，模拟升级前的旧数据）
        let hash = content_hash(b"legacy plaintext data");
        let path = store.object_path(&hash);
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, b"legacy plaintext data").unwrap();

        // read_object 应能读明文
        let read = store.read_object(&hash).unwrap();
        assert_eq!(read, b"legacy plaintext data", "旧明文数据应能被读取（向后兼容）");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn compression_reduces_storage_size() {
        // 验证压缩确实减小存储（重复内容压缩比高）
        let tmp = std::env::temp_dir().join(format!("fs_zstd_size_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // 7000 字节的重复内容（"repeat " × 1000）
        let big = "repeat ".repeat(1000);
        store.write_object(big.as_bytes());

        let stored_size = store.store_size();
        let original_size = big.len() as u64;
        assert!(
            stored_size < original_size,
            "压缩后存储（{} bytes）应小于原始（{} bytes）",
            stored_size, original_size
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_store_count() {
        // Create temp ObjectStore, write known files, verify count
        let tmp = std::env::temp_dir().join(format!("fs_store_count_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let store = ObjectStore::new_at(tmp.join("store"));

        // Empty store -> count = 0
        assert_eq!(store.store_count().unwrap(), 0);

        // Write 3 different contents
        store.write_object(b"content one");
        store.write_object(b"content two");
        store.write_object(b"content three");
        assert_eq!(store.store_count().unwrap(), 3);

        // Write duplicate content -> dedup, count unchanged
        store.write_object(b"content one");
        assert_eq!(store.store_count().unwrap(), 3);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
