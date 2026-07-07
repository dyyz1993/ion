//! ion 文件系统路径管理 —— 完全对齐 pi 的目录结构
//!
//! ## 全局目录 `~/.ion/`
//!
//! ```text
//! ~/.ion/
//! ├── agent/                     ← getAgentDir()
//! │   ├── settings.json          ← 用户设置
//! │   ├── auth.json              ← API Key 存储（权限 600）
//! │   ├── models.json            ← 用户自定义模型定义
//! │   ├── sessions/              ← 会话文件存储（按 cwd 分组）
//! │   │   ├── --hash--cwd--/     ← 每个 cwd 一个子目录
//! │   │   │   ├── session.jsonl  ← 主会话文件（JSONL v3）
//! │   │   │   ├── session.lock   ← 会话锁文件
//! │   │   │   └── data/          ← 扩展的 session 级数据
//! │   │   │       └── <sessionId>/
//! │   │   │           └── <extName>/
//! │   │   ├── --hash--cwd2--/
//! │   │   └── ...
//! │   ├── sessions.index.json    ← 全局会话元数据索引
//! │   ├── extensions-data/       ← 扩展的全局数据（globalDataDir）
//! │   │   └── <extName>/
//! │   ├── project-data/          ← 扩展的项目级数据（projectDataDir）
//! │   │   └── <hash>--<name>/
//! │   │       └── <extName>/
//! │   ├── cwd-data/              ← 扩展的 cwd 级数据（cwdDataDir）
//! │   │   └── <hash>--<name>/
//! │   │       └── <extName>/
//! │   ├── projects/              ← 项目用户状态（skills 等）
//! │   │   └── <hash>--<name>/
//! │   │       └── skills/
//! │   ├── cache/                 ← 缓存
//! │   ├── extensions/            ← 全局扩展
//! │   ├── skills/                ← 全局技能
//! │   ├── prompts/               ← 全局提示模板
//! │   ├── themes/                ← 全局主题
//! │   ├── tools/                 ← 工具目录
//! │   ├── bin/                   ← 托管二进制文件（fd, rg）
//! │   ├── tmp/                   ← 临时文件
//! │   │   ├── extensions/        ← 扩展临时文件
//! │   │   ├── ion-bash-<id>.log  ← Bash 输出溢出
//! │   │   ├── ion-input-<id>.txt ← 大输入溢出
//! │   │   └── ion-tool-results/  ← 工具结果预算溢出
//! │   │       └── <slug>/
//! │   └── last_session           ← 上次会话 ID（纯文本）
//! ├── worktrees/                 ← Git worktree 隔离
//! │   └── <repoName>-<safeBranch>/
//! └── pi-debug.log               ← 调试日志（兼容 pi 命名）
//! ```
//!
//! ## 项目级目录 `<project>/.ion/`
//!
//! ```text
//! <project>/.ion/
//! ├── settings.json              ← 项目级设置（与全局深度合并）
//! ├── extensions/                ← 项目级扩展
//! ├── skills/                    ← 项目级技能
//! ├── prompts/                   ← 项目级提示模板
//! ├── rules/                     ← 规则文件
//! ├── rules-config.json          ← 规则配置
//! └── memory/                    ← 会话记忆
//! ```

use std::path::PathBuf;

/// ion 根目录名称
const ION_DIR: &str = ".ion";

/// 项目级配置目录名称（可通过 package.json 自定义）
const CONFIG_DIR_NAME: &str = ".ion";

// ---------------------------------------------------------------------------
// 全局根目录
// ---------------------------------------------------------------------------

/// ~/.ion/
pub fn root() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(ION_DIR)
}

// ---------------------------------------------------------------------------
// Host 运行时文件（Unix socket / PID）
// ---------------------------------------------------------------------------

/// ~/.ion/host.sock — Host 的 Unix socket 入口
pub fn host_socket_path() -> PathBuf {
    root().join("host.sock")
}

/// ~/.ion/host.pid — Host 的 PID 文件（防重复启动）
pub fn host_pid_path() -> PathBuf {
    root().join("host.pid")
}

/// 检查 Host 是否在运行：读 PID 文件 + 验证进程存活
pub fn host_running() -> Option<u32> {
    let pid_path = host_pid_path();
    // Fallback: check old manager.pid path for migration
    let old_pid_path = root().join("manager.pid");
    if !pid_path.exists() && old_pid_path.exists() {
        let content = std::fs::read_to_string(&old_pid_path).ok()?;
        let pid: u32 = content.trim().parse().ok()?;
        let rc = libc_kill(pid, 0);
        if rc == 0 { Some(pid) } else {
            let _ = std::fs::remove_file(&old_pid_path);
            None
        }
    } else {
        let content = std::fs::read_to_string(&pid_path).ok()?;
        let pid: u32 = content.trim().parse().ok()?;
        let rc = libc_kill(pid, 0);
        if rc == 0 { Some(pid) } else {
            let _ = std::fs::remove_file(&pid_path);
            None
        }
    }
}

// 跨平台 kill(pid, 0)。libc 在所有 unix 上都有；windows 不支持 Unix socket 跳过。
#[cfg(unix)]
fn libc_kill(pid: u32, sig: i32) -> i32 {
    // 直接调 syscall，避免引入 libc crate
    unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid as i32, sig)
    }
}

#[cfg(not(unix))]
fn libc_kill(_pid: u32, _sig: i32) -> i32 { -1 }

// ---------------------------------------------------------------------------
// Agent 目录（核心配置）
// ---------------------------------------------------------------------------

/// ~/.ion/agent/ — 可通过 ION_AGENT_DIR 环境变量覆盖
pub fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ION_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    root().join("agent")
}

/// ~/.ion/agent/settings.json
pub fn settings_path() -> PathBuf {
    agent_dir().join("settings.json")
}

/// ~/.ion/auth.json  (直接在 ~/.ion/ 下，权限 600)
pub fn auth_path() -> PathBuf {
    root().join("auth.json")
}

/// ~/.ion/agent/models.json
pub fn models_path() -> PathBuf {
    agent_dir().join("models.json")
}

// ---------------------------------------------------------------------------
// 会话存储（按 cwd 分组）
// ---------------------------------------------------------------------------

/// ~/.ion/agent/sessions/ — 可通过 ION_SESSION_DIR 环境变量覆盖
pub fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ION_SESSION_DIR") {
        return PathBuf::from(dir);
    }
    agent_dir().join("sessions")
}

/// sessions/--cwd_hash--cwd_name--/
/// 每个 cwd 一个子目录
pub fn session_cwd_dir(cwd: &str) -> PathBuf {
    sessions_dir().join(encode_path(cwd))
}

/// sessions/--cwd_hash--cwd_name--/session.jsonl
/// 主会话文件（JSONL v3 格式）
pub fn session_jsonl_path(cwd: &str) -> PathBuf {
    session_cwd_dir(cwd).join("session.jsonl")
}

/// sessions/--cwd_hash--cwd_name--/session.lock
/// 会话锁文件
pub fn session_lock_path(cwd: &str) -> PathBuf {
    session_cwd_dir(cwd).join("session.lock")
}

/// sessions/--cwd_hash--cwd_name--/data/<sessionId>/<extName>/
/// 扩展的 session 级数据（sessionDataDir）
pub fn session_data_dir(cwd: &str, session_id: &str, ext_name: &str) -> PathBuf {
    session_cwd_dir(cwd)
        .join("data")
        .join(session_id)
        .join(ext_name)
}

/// ~/.ion/agent/sessions.index.json
/// 全局会话元数据索引
pub fn sessions_index_path() -> PathBuf {
    agent_dir().join("sessions.index.json")
}

/// ~/.ion/agent/last_session
/// 上次使用的会话 ID（纯文本）
pub fn last_session_path() -> PathBuf {
    agent_dir().join("last_session")
}

// ---------------------------------------------------------------------------
// 扩展数据目录
// ---------------------------------------------------------------------------

/// ~/.ion/agent/extensions-data/<extName>/
/// 扩展的全局数据（globalDataDir）
pub fn global_data_dir(ext_name: &str) -> PathBuf {
    agent_dir()
        .join("extensions-data")
        .join(ext_name)
}

/// ~/.ion/agent/project-data/<hash>--<name>/<extName>/
/// 扩展的项目级数据（projectDataDir）
pub fn project_data_dir(project_path: &str, ext_name: &str) -> PathBuf {
    agent_dir()
        .join("project-data")
        .join(encode_path(project_path))
        .join(ext_name)
}

/// ~/.ion/agent/cwd-data/<hash>--<name>/<extName>/
/// 扩展的 cwd 级数据（cwdDataDir）
pub fn cwd_data_dir(cwd: &str, ext_name: &str) -> PathBuf {
    agent_dir()
        .join("cwd-data")
        .join(encode_path(cwd))
        .join(ext_name)
}

/// ~/.ion/agent/projects/<hash>--<name>/
/// 项目用户状态（getProjectUserStateDir）
pub fn project_user_state_dir(project_path: &str) -> PathBuf {
    agent_dir()
        .join("projects")
        .join(encode_path(project_path))
}

/// ~/.ion/agent/projects/<hash>--<name>/skills/
/// 项目私有技能（getProjectPrivateSkillsDir）
pub fn project_private_skills_dir(project_path: &str) -> PathBuf {
    project_user_state_dir(project_path).join("skills")
}

/// <project>/.ion/<extName>/
/// 扩展的本地项目数据（写在项目目录里，可 git 提交）
pub fn project_local_data_dir(project_root: &str, ext_name: &str) -> PathBuf {
    project_config_dir(project_root).join(ext_name)
}

// ---------------------------------------------------------------------------
// 扩展/技能/提示/主题/工具/二进制
// ---------------------------------------------------------------------------

/// ~/.ion/agent/extensions/
pub fn extensions_dir() -> PathBuf {
    agent_dir().join("extensions")
}

/// ~/.ion/agent/skills/
pub fn skills_dir() -> PathBuf {
    agent_dir().join("skills")
}

/// ~/.ion/agent/prompts/
pub fn prompts_dir() -> PathBuf {
    agent_dir().join("prompts")
}

/// ~/.ion/agent/themes/
pub fn themes_dir() -> PathBuf {
    agent_dir().join("themes")
}

/// ~/.ion/agent/tools/
pub fn tools_dir() -> PathBuf {
    agent_dir().join("tools")
}

/// ~/.ion/agent/bin/
pub fn bin_dir() -> PathBuf {
    agent_dir().join("bin")
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

/// ~/.ion/agent/cache/
pub fn cache_dir() -> PathBuf {
    agent_dir().join("cache")
}

// ---------------------------------------------------------------------------
// 临时文件
// ---------------------------------------------------------------------------

/// ~/.ion/agent/tmp/
pub fn tmp_dir() -> PathBuf {
    agent_dir().join("tmp")
}

/// ~/.ion/agent/tmp/extensions/
pub fn tmp_extensions_dir() -> PathBuf {
    tmp_dir().join("extensions")
}

/// ~/.ion/agent/tmp/ion-bash-<uuid>.log
pub fn bash_log_path(uuid: &str) -> PathBuf {
    tmp_dir().join(format!("ion-bash-{uuid}.log"))
}

/// ~/.ion/agent/tmp/ion-input-<uuid>.txt
pub fn input_overflow_path(uuid: &str) -> PathBuf {
    tmp_dir().join(format!("ion-input-{uuid}.txt"))
}

/// ~/.ion/agent/tmp/ion-tool-results/<slug>/
pub fn tool_results_dir(slug: &str) -> PathBuf {
    tmp_dir()
        .join("ion-tool-results")
        .join(slug)
}

// ---------------------------------------------------------------------------
// 项目级目录
// ---------------------------------------------------------------------------

/// <project>/.ion/
pub fn project_config_dir(project_root: &str) -> PathBuf {
    PathBuf::from(project_root).join(CONFIG_DIR_NAME)
}

/// <project>/.ion/settings.json
pub fn project_settings_path(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("settings.json")
}

/// <project>/.ion/extensions/
pub fn project_extensions_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("extensions")
}

/// <project>/.ion/skills/
pub fn project_skills_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("skills")
}

/// <project>/.ion/prompts/
pub fn project_prompts_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("prompts")
}

/// <project>/.ion/rules/
pub fn project_rules_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("rules")
}

/// <project>/.ion/rules-config.json
pub fn project_rules_config_path(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("rules-config.json")
}

/// <project>/.ion/memory/
pub fn project_memory_dir(project_root: &str) -> PathBuf {
    project_config_dir(project_root).join("memory")
}

// ---------------------------------------------------------------------------
// Worktree 隔离目录
// ---------------------------------------------------------------------------

/// ~/.ion/worktrees/
pub fn worktree_root() -> PathBuf {
    std::env::var("ION_WORKTREE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root().join("worktrees"))
}

/// ~/.ion/worktrees/<repoName>-<safeBranch>/
pub fn worktree_path(repo_name: &str, safe_branch: &str) -> PathBuf {
    worktree_root().join(format!("{repo_name}-{safe_branch}"))
}

// ---------------------------------------------------------------------------
// 调试日志
// ---------------------------------------------------------------------------

/// ~/.ion/pi-debug.log  (兼容 pi 命名)
pub fn debug_log_path() -> PathBuf {
    root().join("pi-debug.log")
}

// ---------------------------------------------------------------------------
// 系统临时目录（可回收）
// ---------------------------------------------------------------------------

/// 系统临时目录下的 ion 文件
pub fn system_tmp_dir() -> PathBuf {
    std::env::var("ION_TMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// <tmp>/ion-bash-<uuid>.log  (Bash 输出溢出，超过 4KB 时写文件)
pub fn system_bash_log(uuid: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-bash-{uuid}.log"))
}

/// <tmp>/ion-input-<uuid>.txt  (大输入溢出)
pub fn system_input_overflow(uuid: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-input-{uuid}.txt"))
}

/// <tmp>/ion-tool-results/<slug>/  (工具结果预算溢出)
pub fn system_tool_results_dir(slug: &str) -> PathBuf {
    system_tmp_dir()
        .join("ion-tool-results")
        .join(slug)
}

/// <tmp>/ion-clipboard-<uuid>.<ext>  (剪贴板粘贴图片)
pub fn system_clipboard_path(uuid: &str, ext: &str) -> PathBuf {
    system_tmp_dir().join(format!("ion-clipboard-{uuid}.{ext}"))
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 创建所有需要的目录（首次运行调用）
pub fn ensure_dirs() {
    let dirs = [
        agent_dir(),
        sessions_dir(),
        extensions_dir(),
        skills_dir(),
        prompts_dir(),
        themes_dir(),
        tools_dir(),
        bin_dir(),
        cache_dir(),
        tmp_dir(),
        tmp_extensions_dir(),
        agent_dir().join("extensions-data"),
        agent_dir().join("project-data"),
        agent_dir().join("cwd-data"),
        agent_dir().join("projects"),
        worktree_root(),
        root().join("worktrees"),
    ];
    for dir in &dirs {
        let _ = std::fs::create_dir_all(dir);
    }
}

// ---------------------------------------------------------------------------
// 路径编码（对齐 pi 的 --hash--name-- 格式）
// ---------------------------------------------------------------------------

/// 编码路径名为安全的目录名（对齐 pi 的 --hash--name-- 格式）
pub fn encode_path(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    format!("--{hash:x}--{name}--")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_under_dot_ion() {
        let r = root();
        assert!(r.to_str().unwrap().contains(".ion"));
    }

    #[test]
    fn agent_dir_ends_with_agent() {
        let d = agent_dir();
        assert!(d.to_str().unwrap().ends_with("agent"));
    }

    #[test]
    fn session_path_has_cwd_hash() {
        let p = session_jsonl_path("/Users/test/my-project");
        let s = p.to_str().unwrap();
        assert!(s.contains("--"));
        assert!(s.ends_with("session.jsonl"));
        assert!(s.contains("sessions"));
    }

    #[test]
    fn session_cwd_dir_format() {
        let d = session_cwd_dir("/tmp/foo");
        let s = d.to_str().unwrap();
        assert!(s.starts_with(&sessions_dir().to_str().unwrap()));
        assert!(s.contains("--"));
        assert!(s.ends_with("--foo--"));
    }

    #[test]
    fn worktree_path_format() {
        let p = worktree_path("my-repo", "feature/abc");
        assert!(p.to_str().unwrap().ends_with("my-repo-feature/abc"));
    }

    #[test]
    fn encode_path_is_deterministic() {
        let a = encode_path("/Users/test/project");
        let b = encode_path("/Users/test/project");
        assert_eq!(a, b);
        assert!(a.starts_with("--"));
    }

    #[test]
    fn project_config_under_dot_ion() {
        let p = project_settings_path("/tmp/myproject");
        assert!(p.to_str().unwrap().ends_with(".ion/settings.json"));
    }

    #[test]
    fn global_data_dir_format() {
        let d = global_data_dir("my-ext");
        assert!(d.to_str().unwrap().ends_with("my-ext"));
        assert!(d.to_str().unwrap().contains("extensions-data"));
    }

    #[test]
    fn project_data_dir_format() {
        let d = project_data_dir("/root/proj", "ext1");
        assert!(d.to_str().unwrap().contains("ext1"));
    }

    #[test]
    fn cwd_data_dir_format() {
        let d = cwd_data_dir("/tmp/work", "ext1");
        assert!(d.to_str().unwrap().contains("ext1"));
    }

    #[test]
    fn project_user_state_has_skills() {
        let d = project_private_skills_dir("/p");
        assert!(d.to_str().unwrap().ends_with("skills"));
    }

    #[test]
    fn project_has_rules_and_memory() {
        assert!(project_rules_dir("/p").to_str().unwrap().ends_with("rules"));
        assert!(project_memory_dir("/p").to_str().unwrap().ends_with("memory"));
    }

    #[test]
    fn themes_tools_bin_dirs() {
        assert!(themes_dir().to_str().unwrap().ends_with("themes"));
        assert!(tools_dir().to_str().unwrap().ends_with("tools"));
        assert!(bin_dir().to_str().unwrap().ends_with("bin"));
    }

    #[test]
    fn system_tmp_has_ion_prefix() {
        let log = system_bash_log("abc123");
        assert!(log.to_str().unwrap().contains("ion-bash-abc123"));
    }

    #[test]
    fn debug_log_is_debug_log() {
        assert!(debug_log_path().to_str().unwrap().ends_with("pi-debug.log"));
    }

    #[test]
    fn session_data_dir_format() {
        let d = session_data_dir("/p", "sess-1", "ext-a");
        let s = d.to_str().unwrap();
        assert!(s.contains("sess-1"));
        assert!(s.contains("ext-a"));
        assert!(s.contains("data"));
    }

    #[test]
    fn ensure_dirs_creates_all() {
        ensure_dirs();
        assert!(agent_dir().exists());
        assert!(sessions_dir().exists());
        assert!(cache_dir().exists());
        assert!(tmp_dir().exists());
        assert!(worktree_root().exists());
        assert!(extensions_dir().exists());
        assert!(themes_dir().exists());
        assert!(tools_dir().exists());
        assert!(bin_dir().exists());
    }
}
