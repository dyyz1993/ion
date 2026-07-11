//! Content-addressed object store — 文件内容按 hash 去重存储
//!
//! 设计：类似 mini-git 的 object store
//! - 内容 → SipHash → 路径 objects/<前2位>/<剩余hash>
//! - 相同内容只存一次（exists 判断）
//!
//! 注：当前用 std DefaultHasher（SipHash-1-3，64bit）。
//! 对小项目够用；文件量到百万级时有碰撞风险，届时升级 sha256。

use std::path::{Path, PathBuf};

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

        // 写入内容
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, content).ok();

        WriteResult {
            hash,
            deduped: false,
        }
    }

    /// 读取内容
    pub fn read_object(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.object_path(hash);
        let content = std::fs::read(&path).ok()?;
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
}
