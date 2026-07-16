//! StorageContext — 统一存储路径访问抽象
//!
//! 所有扩展通过 StorageContext 拿 5 维路径，不用自己 import paths 现算。
//! StorageContext 不认识任何具体扩展——它只是路径计算器，扩展自己传 ext_name。
//!
//! 使用方式：
//! ```ignore
//! // ion_worker.rs 构造时创建
//! let storage = StorageContext::new(&worker_cwd, &sid, &config_root);
//!
//! // 传给扩展
//! let bash_ext = BashExtension::new(storage.clone());
//!
//! // 扩展内部使用
//! let path = self.storage.session_dir("bash");
//! ```

use std::path::PathBuf;

/// 统一存储访问上下文 — 扩展通过它拿 5 维路径
///
/// 构造时由 ion_worker.rs 创建（cwd + session_id + config_root），
/// 传给每个扩展的 new()。扩展 self 持有，方法里 self.storage.xxx() 拿路径。
#[derive(Clone, Debug)]
pub struct StorageContext {
    /// worker 的 cwd（worktree 场景 = worktree 路径，用于扫描文件 + session 隔离 key）
    pub cwd: String,
    /// 当前 session ID（用于 ④ session 维度隔离）
    pub session_id: String,
    /// 项目根（ION_PROJECT_ROOT 优先，用于读主仓库 .ion/ 资源）
    pub config_root: String,
}

impl StorageContext {
    pub fn new(cwd: &str, session_id: &str, config_root: &str) -> Self {
        Self {
            cwd: cwd.to_string(),
            session_id: session_id.to_string(),
            config_root: config_root.to_string(),
        }
    }

    // ── ① 全局维度：所有项目共享 ──

    /// ① 全局：`~/.ion/agent/extensions-data/<ext>/`
    pub fn global_dir(&self, ext_name: &str) -> PathBuf {
        crate::paths::global_data_dir(ext_name)
    }

    // ── ② 项目维度：worktree 共享（git common dir hash）──

    /// ② 项目：`~/.ion/agent/project-data/<git_key>/<ext>/`
    /// 主仓库和 worktree 算出同一个 key → 共享存储
    pub fn project_dir(&self, ext_name: &str) -> PathBuf {
        crate::paths::project_data_dir(&self.cwd, ext_name)
    }

    // ── ③ 仓库内维度：走 git checkout（worktree 回源 config_root）──

    /// ③ 仓库内：`<config_root>/.ion/<ext>/`
    /// worktree 通过 ION_PROJECT_ROOT 回源主仓库
    pub fn project_local_dir(&self, ext_name: &str) -> PathBuf {
        crate::paths::project_local_data_dir(&self.config_root, ext_name)
    }

    /// CWD 级：`~/.ion/agent/cwd-data/<encoded-cwd>/<ext>/`（worktree 独立）
    pub fn cwd_dir(&self, ext_name: &str) -> PathBuf {
        crate::paths::cwd_data_dir(&self.cwd, ext_name)
    }

    // ── ④ Session 维度：session 隔离 ──

    /// ④ Session：`sessions/<hash>/data/<sid>/<ext>/`
    pub fn session_dir(&self, ext_name: &str) -> PathBuf {
        crate::paths::session_data_dir(&self.cwd, &self.session_id, ext_name)
    }

    // ── 特殊约定路径（扩展按需使用）──

    /// Bash 后台进程存储：④ session 级
    pub fn bash_processes_path(&self) -> PathBuf {
        crate::paths::bash_processes_path(&self.cwd, &self.session_id)
    }

    /// Session JSONL 路径：④ session 级
    pub fn session_jsonl_path(&self) -> PathBuf {
        crate::paths::session_jsonl_path(&self.cwd)
    }

    /// 项目级 settings.json：③ 仓库内（worktree 回源 config_root）
    pub fn project_settings_path(&self) -> PathBuf {
        crate::paths::project_settings_path(&self.config_root)
    }

    /// 全局 settings.json：① 全局
    pub fn global_settings_path(&self) -> PathBuf {
        crate::paths::settings_path()
    }

    /// File Store 目录：② 项目级（git_key）
    pub fn file_store_dir(&self, project_key: &str) -> PathBuf {
        crate::paths::file_store_dir(project_key)
    }

    /// 项目维度配置目录：② 项目级（git_key）
    pub fn project_dimension_dir(&self, project_key: &str) -> PathBuf {
        crate::paths::project_dimension_dir(project_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_context_5_dimensions() {
        let ctx = StorageContext::new("/tmp/proj", "sess_001", "/tmp/proj");

        // ① 全局
        let g = ctx.global_dir("myext");
        assert!(g.to_str().unwrap().contains("extensions-data"));
        assert!(g.to_str().unwrap().ends_with("myext"));

        // ② 项目
        let p = ctx.project_dir("myext");
        assert!(p.to_str().unwrap().contains("project-data"));
        assert!(p.to_str().unwrap().ends_with("myext"));

        // ③ 仓库内
        let pl = ctx.project_local_dir("myext");
        assert!(pl.to_str().unwrap().contains(".ion"));
        assert!(pl.to_str().unwrap().ends_with("myext"));

        // ④ Session
        let s = ctx.session_dir("myext");
        assert!(s.to_str().unwrap().contains("sess_001"));
        assert!(s.to_str().unwrap().ends_with("myext"));
    }

    #[test]
    fn storage_context_bash_path_is_session_level() {
        let ctx = StorageContext::new("/tmp/proj", "sess_001", "/tmp/proj");
        let p = ctx.bash_processes_path();
        assert!(p.to_str().unwrap().ends_with("processes.json"));
        assert!(p.to_str().unwrap().contains("sess_001"));
    }

    #[test]
    fn storage_context_clone_is_independent() {
        let ctx = StorageContext::new("/tmp/proj", "sess_001", "/tmp/proj");
        let ctx2 = ctx.clone();
        assert_eq!(ctx.cwd, ctx2.cwd);
        assert_eq!(ctx.session_id, ctx2.session_id);
    }

    #[test]
    fn storage_context_different_sessions_isolated() {
        let ctx1 = StorageContext::new("/tmp/proj", "sess_001", "/tmp/proj");
        let ctx2 = StorageContext::new("/tmp/proj", "sess_002", "/tmp/proj");
        assert_ne!(ctx1.session_dir("ext"), ctx2.session_dir("ext"));
        assert_ne!(ctx1.bash_processes_path(), ctx2.bash_processes_path());
    }

    #[test]
    fn storage_context_does_not_know_extension_names() {
        // StorageContext 不应该有任何硬编码的扩展名
        let ctx = StorageContext::new("/tmp", "s1", "/tmp");
        let _ = ctx.global_dir("any_ext");
        let _ = ctx.project_dir("any_ext");
        let _ = ctx.session_dir("any_ext");
        let _ = ctx.project_local_dir("any_ext");
    }
}
