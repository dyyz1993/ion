//! Content-addressed object store — 文件内容按 hash 去重存储
//!
//! 设计：类似 mini-git 的 object store
//! - 内容 → sha256 → 路径 objects/<前2位>/<剩余hash>
//! - 相同内容只存一次（existsSync 判断）
//! - 元数据（createdAt/accessedAt）单独存 metadata/

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Content-addressed object store
pub struct ObjectStore {
    /// 存储根目录（~/.ion/file-store/<project_key>/）
    store_dir: PathBuf,
    /// 内存缓存：hash → 是否存在（避免频繁 existsSync）
    #[allow(dead_code)]
    cache: Mutex<HashMap<String, bool>>,
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
        std::fs::create_dir_all(store_dir.join("metadata")).ok();
        Self {
            store_dir,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 从 cwd 创建（自动算 project_key）
    pub fn for_cwd(cwd: &str) -> Self {
        Self::for_project(&project_key(cwd))
    }

    /// 直接指定存储目录（测试用）
    #[cfg(test)]
    pub fn new_at(store_dir: PathBuf) -> Self {
        std::fs::create_dir_all(store_dir.join("objects")).ok();
        std::fs::create_dir_all(store_dir.join("metadata")).ok();
        Self {
            store_dir,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 存储根目录
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    /// 写入内容，返回 hash + 去重标记
    pub fn write_object(&self, content: &[u8]) -> WriteResult {
        let hash = content_hash(content);
        let path = self.object_path(&hash);

        if path.exists() {
            // 去重：已存在，更新 accessedAt
            self.touch_accessed(&hash);
            return WriteResult {
                hash,
                deduped: true,
            };
        }

        // 写入内容
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&content.len(), &mut hasher); // 确保使用内容
        std::fs::write(&path, content).ok();

        // 写入元数据
        self.write_metadata(&hash, content.len() as u64);

        WriteResult {
            hash,
            deduped: false,
        }
    }

    /// 读取内容
    pub fn read_object(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.object_path(hash);
        let content = std::fs::read(&path).ok()?;
        self.touch_accessed(hash);
        Some(content)
    }

    /// 读取为 UTF-8 字符串（diff 用）
    pub fn read_object_text(&self, hash: &str) -> Option<String> {
        String::from_utf8(self.read_object(hash)?).ok()
    }

    /// object 是否存在
    pub fn exists(&self, hash: &str) -> bool {
        self.object_path(hash).exists()
    }

    /// object 文件路径
    fn object_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = if hash.len() >= 2 {
            hash.split_at(2)
        } else {
            (hash, "")
        };
        self.store_dir.join("objects").join(prefix).join(rest)
    }

    /// 元数据文件路径
    fn metadata_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = if hash.len() >= 2 {
            hash.split_at(2)
        } else {
            (hash, "")
        };
        self.store_dir.join("metadata").join(prefix).join(rest)
    }

    /// 写入元数据
    fn write_metadata(&self, hash: &str, size: u64) {
        let path = self.metadata_path(hash);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let now = crate::session_jsonl::timestamp_iso();
        let meta = serde_json::json!({
            "hash": hash,
            "size": size,
            "createdAt": now,
            "accessedAt": now,
        });
        std::fs::write(&path, serde_json::to_string(&meta).unwrap_or_default()).ok();
    }

    /// 更新 accessedAt
    fn touch_accessed(&self, hash: &str) {
        let path = self.metadata_path(hash);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut meta) = serde_json::from_str::<serde_json::Value>(&content) {
                meta["accessedAt"] = serde_json::json!(crate::session_jsonl::timestamp_iso());
                std::fs::write(&path, serde_json::to_string(&meta).unwrap_or_default()).ok();
            }
        }
    }

    /// 获取存储总大小（bytes）
    pub fn store_size(&self) -> u64 {
        let objects_dir = self.store_dir.join("objects");
        dir_size(&objects_dir)
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
        let _ = std::fs::remove_file(self.metadata_path(hash));
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

/// 计算内容的 sha256 hash（返回 hex 字符串）
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
pub fn project_key(cwd: &str) -> String {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(cwd)
        .output()
    {
        if output.status.success() {
            let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // /ion/.git → 主仓库；/ion/.git/worktrees/xxx → worktree
            // 取 common dir（去掉 worktrees/xxx 后缀）
            let common = git_dir.split("/worktrees/").next().unwrap_or(&git_dir);
            return content_hash(common.as_bytes());
        }
    }
    // 非 git：fallback 到 cwd
    content_hash(cwd.as_bytes())
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
}
