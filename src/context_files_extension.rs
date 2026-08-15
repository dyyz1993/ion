//! Context Files Extension (AGENTS.md / CLAUDE.md loading)
//!
//! Loads project context files (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`)
//! by walking **upward** from the project root until the git root is reached.
//! All discovered files are loaded together (multi-level: root + leaf).
//! Injected into the system prompt via the `on_system_prompt` hook.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::agent::error::AgentResult;
use crate::agent::extension::Extension;

const DEFAULT_MAX_CHARS: usize = 12_000;
const MAX_UPWARD_LEVELS: usize = 5;
const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "GEMINI.md"];

#[derive(Clone, Debug)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
    pub level: usize,
}

pub struct ContextFilesExtension {
    project_dir: PathBuf,
}

impl ContextFilesExtension {
    pub fn new() -> Self {
        Self {
            project_dir: crate::paths::project_root_for_config(),
        }
    }

    pub fn with_project_dir(project_dir: PathBuf) -> Self {
        Self { project_dir }
    }

    pub fn is_disabled_by_env() -> bool {
        std::env::var("ION_NO_CONTEXT_FILES")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Walk upward from project_dir, collecting ALL context files found at every level.
    /// Same directory: only first hit (AGENTS > CLAUDE > GEMINI).
    /// Multi-level: ALL levels loaded (root + intermediate + leaf).
    /// Stops at .git boundary or MAX_UPWARD_LEVELS.
    pub fn load_context_files(&self) -> Vec<ContextFile> {
        let mut files: Vec<ContextFile> = Vec::new();
        let mut dir: Option<&Path> = Some(&self.project_dir);
        let mut level = 0usize;

        while let Some(current) = dir {
            if level > MAX_UPWARD_LEVELS {
                break;
            }
            for name in CONTEXT_FILE_NAMES {
                let candidate = current.join(name);
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    files.push(ContextFile {
                        path: candidate.clone(),
                        content,
                        level,
                    });
                    break; // same dir: first hit wins
                }
            }
            if current.join(".git").exists() {
                break; // stop at git root
            }
            dir = current.parent();
            level += 1;
        }

        files.reverse(); // root first, leaf last
        files
    }

    pub fn format_context_block(files: &[ContextFile]) -> String {
        if files.is_empty() {
            return String::new();
        }
        let max_chars = std::env::var("ION_CONTEXT_FILES_MAX_CHARS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&limit| limit > 0)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let mut block = String::from("\n\n--- project context ---\n");
        block.push_str(
            "The following project context files define conventions and rules. \
             You MUST follow them when working in this project.\n\n",
        );

        let mut remaining = max_chars;
        for (i, f) in files.iter().enumerate() {
            let file_name = f
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("context.md");
            if remaining == 0 {
                block.push_str(&format!(
                    "\n## {} (from {})\n[omitted — context-files char budget reached]\n",
                    file_name,
                    f.path.display()
                ));
                continue;
            }
            let content = if f.content.len() <= remaining {
                f.content.clone()
            } else {
                let mut end = remaining;
                while !f.content.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                format!(
                    "{}\n\n[... truncated: context-files char budget ({} chars) reached ...]",
                    &f.content[..end],
                    max_chars
                )
            };
            remaining = remaining.saturating_sub(content.len());

            block.push_str(&format!(
                "## {} (from {})\n\n{}\n\n",
                file_name,
                f.path.display(),
                content.trim()
            ));
            if i + 1 < files.len() {
                block.push_str("---\n\n");
            }
        }
        block
    }
}

impl Default for ContextFilesExtension {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extension for ContextFilesExtension {
    fn name(&self) -> &str {
        "context-files"
    }

    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        let files = self.load_context_files();
        if files.is_empty() {
            return Ok(());
        }
        prompt.push_str(&Self::format_context_block(&files));
        Ok(())
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        match method {
            "list" => {
                let files = self.load_context_files();
                let entries: Vec<serde_json::Value> = files
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "path": f.path.to_string_lossy(),
                            "chars": f.content.len(),
                            "level": f.level,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "files": entries }))
            }
            _ => Err(crate::agent::error::AgentError::Tool(
                "extension rpc method not found".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ion_ctxfiles_{}_{}",
            tag,
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_load_single_agents_md_at_root() {
        let root = temp_dir("single");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "# Rules\nUse extension not plugin.");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("extension not plugin"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_walk_upward_finds_root_agents_md() {
        let root = temp_dir("upward");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "# Root conventions");
        let subdir = root.join("src").join("agent");
        fs::create_dir_all(&subdir).unwrap();
        let ext = ContextFilesExtension::with_project_dir(subdir);
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("AGENTS.md"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_priority_agents_before_claude() {
        let root = temp_dir("priority");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "agents content");
        write(&root.join("CLAUDE.md"), "claude content");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("agents content"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_claude_md_fallback() {
        let root = temp_dir("fallback");
        write(&root.join(".git"), "");
        write(&root.join("CLAUDE.md"), "claude only");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("claude only"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_multi_level_root_first_ordering() {
        let root = temp_dir("multi");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "ROOT");
        let mid = root.join("packages");
        write(&mid.join("AGENTS.md"), "MID");
        let ext = ContextFilesExtension::with_project_dir(mid);
        let files = ext.load_context_files();
        assert_eq!(files.len(), 2);
        assert!(files[0].content.contains("ROOT"));
        assert!(files[1].content.contains("MID"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_no_files_returns_empty() {
        let root = temp_dir("empty");
        write(&root.join(".git"), ""); // git 根阻止向上查找，避免 temp_dir 父目录污染
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        assert!(ext.load_context_files().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_format_block_empty() {
        assert!(ContextFilesExtension::format_context_block(&[]).is_empty());
    }

    #[test]
    fn test_format_block_contains_marker() {
        let f = ContextFile {
            path: PathBuf::from("/tmp/AGENTS.md"),
            content: "# Hello\nWorld".into(),
            level: 0,
        };
        let block = ContextFilesExtension::format_context_block(&[f]);
        assert!(block.contains("--- project context ---"));
        assert!(block.contains("Hello"));
    }

    #[test]
    fn test_truncation_respects_char_boundary() {
        let big: String = "中".repeat(5000);
        let f = ContextFile {
            path: PathBuf::from("/tmp/AGENTS.md"),
            content: big,
            level: 0,
        };
        unsafe {
            std::env::remove_var("ION_CONTEXT_FILES_MAX_CHARS");
        }
        let block = ContextFilesExtension::format_context_block(&[f]);
        assert!(block.contains("truncated"));
    }

    #[test]
    fn test_is_disabled_by_env() {
        unsafe {
            std::env::remove_var("ION_NO_CONTEXT_FILES");
        }
        assert!(!ContextFilesExtension::is_disabled_by_env());
        unsafe {
            std::env::set_var("ION_NO_CONTEXT_FILES", "1");
        }
        assert!(ContextFilesExtension::is_disabled_by_env());
        unsafe {
            std::env::remove_var("ION_NO_CONTEXT_FILES");
        }
    }

    #[tokio::test]
    async fn test_on_system_prompt_injects() {
        let root = temp_dir("inject");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "# Rules\nUse extension.");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let mut prompt = String::from("base");
        ext.on_system_prompt(&mut prompt).await.unwrap();
        assert!(prompt.contains("--- project context ---"));
        assert!(prompt.contains("Use extension."));
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_on_system_prompt_noop_when_empty() {
        let root = temp_dir("noop");
        write(&root.join(".git"), ""); // git 根阻止向上查找
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let mut prompt = String::from("base");
        ext.on_system_prompt(&mut prompt).await.unwrap();
        assert_eq!(prompt, "base");
        let _ = fs::remove_dir_all(&root);
    }

    // ── 补充边界测试 ──

    #[test]
    fn test_three_level_nested_all_loaded() {
        let root = temp_dir("three");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "L0_ROOT");
        let mid = root.join("a").join("b");
        fs::create_dir_all(&mid).unwrap();
        write(&mid.join("AGENTS.md"), "L1_MID");
        let leaf = mid.join("c");
        fs::create_dir_all(&leaf).unwrap();
        write(&leaf.join("AGENTS.md"), "L2_LEAF");
        let ext = ContextFilesExtension::with_project_dir(leaf);
        let files = ext.load_context_files();
        assert_eq!(files.len(), 3, "三级目录应加载 3 个文件");
        assert!(files[0].content.contains("L0_ROOT"), "根在最前");
        assert!(files[1].content.contains("L1_MID"), "中间在第二");
        assert!(files[2].content.contains("L2_LEAF"), "叶子在最后");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_empty_agents_md_file() {
        let root = temp_dir("emptymd");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1, "空文件也加载");
        assert!(files[0].content.is_empty());
        let block = ContextFilesExtension::format_context_block(&files);
        assert!(block.contains("--- project context ---"), "空文件也有段落头");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_single_line_agents_md() {
        let root = temp_dir("oneline");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "just one line");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "just one line");
        let block = ContextFilesExtension::format_context_block(&files);
        assert!(block.contains("just one line"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_claude_before_gemini_priority() {
        let root = temp_dir("cg");
        write(&root.join(".git"), "");
        write(&root.join("CLAUDE.md"), "claude wins");
        write(&root.join("GEMINI.md"), "gemini loses");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1, "同目录只取第一个命中");
        assert!(files[0].content.contains("claude wins"), "CLAUDE 优先于 GEMINI");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_gemini_md_fallback() {
        let root = temp_dir("gemini");
        write(&root.join(".git"), "");
        write(&root.join("GEMINI.md"), "gemini only content");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("gemini only content"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_git_directory_not_file() {
        let root = temp_dir("gitdir");
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        write(&git_dir.join("HEAD"), "ref: refs/heads/main");
        write(&root.join("AGENTS.md"), "real git dir");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1, ".git 是目录也能识别为 git 根");
        assert!(files[0].content.contains("real git dir"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_stops_at_git_boundary_upward() {
        // 结构化验证：git 根有 AGENTS.md，从 git 根的子目录跑，只加载 git 根内的。
        // （不在 .. 写文件，避免污染其他测试的 temp_dir 父目录）
        let root = temp_dir("boundary");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "INSIDE_GIT");
        let leaf = root.join("src").join("deep");
        fs::create_dir_all(&leaf).unwrap();
        let ext = ContextFilesExtension::with_project_dir(leaf);
        let files = ext.load_context_files();
        assert_eq!(files.len(), 1, "只加载 git 根内的");
        assert!(files[0].content.contains("INSIDE_GIT"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_max_upward_levels_limit() {
        let root = temp_dir("deeplevel");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "ROOT_BUT_TOO_DEEP");
        let mut deep = root.clone();
        for _ in 0..10 {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).unwrap();
        let ext = ContextFilesExtension::with_project_dir(deep);
        let files = ext.load_context_files();
        let total_chars: usize = files.iter().map(|f| f.content.len()).sum();
        assert!(total_chars < 100, "层级太深（>5），根 AGENTS.md 不该被加载");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_large_budget_no_truncation() {
        let root = temp_dir("large");
        write(&root.join(".git"), "");
        let big: String = "A".repeat(5000);
        write(&root.join("AGENTS.md"), &big);
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let files = ext.load_context_files();
        unsafe { std::env::set_var("ION_CONTEXT_FILES_MAX_CHARS", "50000"); }
        let block = ContextFilesExtension::format_context_block(&files);
        unsafe { std::env::remove_var("ION_CONTEXT_FILES_MAX_CHARS"); }
        assert!(!block.contains("truncated"), "大预算不截断");
        assert!(block.contains(&"A".repeat(100)), "内容完整");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_multi_file_shared_budget() {
        let root = temp_dir("shared");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), &"X".repeat(400));
        let sub = root.join("pkg");
        fs::create_dir_all(&sub).unwrap();
        write(&sub.join("AGENTS.md"), &"Y".repeat(400));
        let ext = ContextFilesExtension::with_project_dir(sub);
        let files = ext.load_context_files();
        assert_eq!(files.len(), 2);
        unsafe { std::env::set_var("ION_CONTEXT_FILES_MAX_CHARS", "500"); }
        let block = ContextFilesExtension::format_context_block(&files);
        unsafe { std::env::remove_var("ION_CONTEXT_FILES_MAX_CHARS"); }
        assert!(block.contains("truncated"), "总预算 500 时第二个文件被截断");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_on_system_prompt_appends_not_replaces() {
        let root = temp_dir("append");
        write(&root.join(".git"), "");
        write(&root.join("AGENTS.md"), "# Rules");
        let ext = ContextFilesExtension::with_project_dir(root.clone());
        let original = "You are a helpful assistant.";
        let mut prompt = String::from(original);
        ext.on_system_prompt(&mut prompt).await.unwrap_or(());
        assert!(prompt.starts_with(original), "原有 prompt 保留");
        assert!(prompt.contains("--- project context ---"), "追加了 context");
        let _ = fs::remove_dir_all(&root);
    }
}
