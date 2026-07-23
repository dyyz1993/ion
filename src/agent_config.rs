use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Parsed agent configuration from --agent flag or .md file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u64>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub variables: Option<HashMap<String, String>>,
    #[serde(default)]
    pub hooks: Option<HashMap<String, Vec<HookDef>>>,
    /// Workflow gate 配置（内核级 gate 校验）
    #[serde(default)]
    pub workflow: Option<crate::agent::workflow_extension::WorkflowGateConfig>,
    /// The system prompt (markdown body, after frontmatter).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Source for discovery priority.
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HookDef {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default)]
        async_hook: bool,
        #[serde(default)]
        once: bool,
        #[serde(default)]
        timeout: Option<u64>,
    },
    #[serde(rename = "prompt")]
    Prompt {
        prompt: String,
        #[serde(default)]
        once: bool,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

// ---------------------------------------------------------------------------
// Built-in agents
// ---------------------------------------------------------------------------

/// improver 的 prompt 用 include_str! 编译期嵌入，命中 builtin fallback（搜索路径 #4）。
/// 改 examples/agents/improver.md 后无需手动 cp，重新 build 即生效。
const IMPROVER_MD: &str = include_str!("../examples/agents/improver.md");

pub fn builtin_agents() -> Vec<AgentConfig> {
    // improver 从 .md 解析；解析失败兜底一个最小配置（不会让 builtin 列表整个崩）
    let improver = parse_agent_md(IMPROVER_MD, "examples/agents/improver.md")
        .unwrap_or_else(|| AgentConfig {
            name: "improver".into(),
            description: "通用任务智能体（builtin fallback）".into(),
            tools: None,
            disallowed_tools: None,
            model: None,
            max_turns: None,
            thinking_level: Some("high".into()),
            tier: None,
            color: Some("cyan".into()),
            background: None,
            skills: None,
            variables: None,
            hooks: None,
            system_prompt: None,
            workflow: None,
            source: "builtin".into(),
        });

    vec![
        AgentConfig {
            name: "build".into(),
            description: "Full-stack development with read, write, edit and execution capabilities".into(),
            tools: None,
            disallowed_tools: None,
            model: None,
            max_turns: None,
            thinking_level: None,
            tier: Some("pro".into()),
            color: Some("orange".into()),
            background: None,
            skills: None,
            variables: None,
            hooks: None,
            system_prompt: None,
            workflow: None,
            source: "builtin".into(),
        },
        AgentConfig {
            name: "explore".into(),
            description: "Read-only exploration, search and read code".into(),
            tools: Some(vec!["read".into(), "grep".into(), "find".into(), "ls".into(), "bash".into()]),
            disallowed_tools: Some(vec!["edit".into(), "write".into()]),
            model: None,
            max_turns: None,
            thinking_level: None,
            tier: Some("fast".into()),
            color: Some("blue".into()),
            background: None,
            skills: None,
            variables: None,
            hooks: None,
            system_prompt: Some("You are in read-only exploration mode. You can read, search, and list files, but you must NOT edit or write any files.".into()),
            workflow: None,
            source: "builtin".into(),
        },
        AgentConfig {
            name: "plan".into(),
            description: "Planning mode — output analysis and specifications only".into(),
            tools: Some(vec!["read".into(), "grep".into(), "find".into(), "ls".into()]),
            disallowed_tools: Some(vec!["edit".into(), "write".into(), "bash".into()]),
            model: None,
            max_turns: None,
            thinking_level: Some("high".into()),
            tier: Some("max".into()),
            color: Some("purple".into()),
            background: None,
            skills: None,
            variables: None,
            hooks: None,
            system_prompt: Some("You are in planning mode. Output analysis, architecture decisions, and implementation plans. Do NOT edit any files.".into()),
            workflow: None,
            source: "builtin".into(),
        },
        improver,
    ]
}

/// Count the number of builtin agents.
pub fn count_builtin_agents() -> usize {
    builtin_agents().len()
}

// ---------------------------------------------------------------------------
// Discovery paths
// ---------------------------------------------------------------------------

pub fn global_agents_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ion").join("agent").join("agents")
}

pub fn project_agents_dir() -> Option<PathBuf> {
    // 优先用 ION_PROJECT_ROOT（worktree 场景回源主仓库，缺口 #2）
    let base = std::env::var("ION_PROJECT_ROOT")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .ok()?;
    Some(base.join(".ion").join("agents"))
}

// ---------------------------------------------------------------------------
// Parse agent from .md file
// ---------------------------------------------------------------------------

pub fn parse_agent_file(path: &PathBuf) -> Option<AgentConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_agent_md(&content, &path.to_string_lossy())
}

pub fn parse_agent_md(content: &str, file_path: &str) -> Option<AgentConfig> {
    if !content.starts_with("---") {
        return None;
    }
    let end = content[3..].find("---")?;
    let frontmatter_str = &content[3..3 + end];
    let body = content[3 + end + 3..].trim();

    let mut config: AgentConfig = serde_yaml::from_str(frontmatter_str).ok()?;
    config.system_prompt = Some(if body.is_empty() { config.description.clone() } else { body.to_string() });
    config.source = if file_path.contains(".ion/agents/") {
        "project".into()
    } else if file_path.contains("examples/agents/") {
        "examples".into()
    } else {
        "user".into()
    };
    Some(config)
}

// ---------------------------------------------------------------------------
// Find agent by name or path
// ---------------------------------------------------------------------------

/// Look for a shipped agent file (e.g. `user.md`) under an `examples/agents/`
/// directory. Search order:
///   1. `$CARGO_MANIFEST_DIR/examples/agents/` (compile-time path, most reliable)
///   2. `$cwd/examples/agents/`
///   3. Walk up from `$cwd` to find the nearest ancestor containing
///      `examples/agents/` (covers subdirectory invocations).
/// Returns the absolute path to the agent file if found.
fn find_examples_agent(filename: &str) -> Option<PathBuf> {
    let candidate_dirs = examples_agent_dirs();
    for dir in candidate_dirs {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Build the list of candidate `examples/agents/` directories to probe.
fn examples_agent_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // 1. Compile-time manifest dir (preferred — stable regardless of cwd).
    //    Only available when built with cargo (sets CARGO_MANIFEST_DIR).
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        dirs.push(PathBuf::from(manifest).join("examples").join("agents"));
    }

    // 2. Current working directory, then walk up to project root.
    if let Ok(cwd) = std::env::current_dir() {
        // cwd itself
        dirs.push(cwd.join("examples").join("agents"));

        // Walk up: check each ancestor for an examples/agents/ folder.
        let mut ancestor = cwd.parent();
        while let Some(parent) = ancestor {
            let dir = parent.join("examples").join("agents");
            if dir.exists() {
                dirs.push(dir);
            }
            ancestor = parent.parent();
        }
    }

    dirs
}

pub fn find_agent(name_or_path: &str) -> Option<AgentConfig> {
    // 1. If it's a file path, load directly
    let path = PathBuf::from(name_or_path);
    if path.exists() && name_or_path.ends_with(".md") {
        return parse_agent_file(&path);
    }

    // 2. Project .ion/agents/ (highest priority — most specific to current project)
    if let Some(proj) = project_agents_dir() {
        let proj_path = proj.join(format!("{name_or_path}.md"));
        if proj_path.exists() {
            return parse_agent_file(&proj_path);
        }
    }

    // 2.5. examples/agents/ (shipped agents — user, architect, qa, pm, ci, maintainer, etc.)
    // Check the current working directory first, then walk up to find the project root
    // (the directory containing examples/agents/).
    if let Some(examples_path) = find_examples_agent(&format!("{name_or_path}.md")) {
        return parse_agent_file(&examples_path);
    }

    // 3. Global ~/.ion/agent/agents/
    let global = global_agents_dir().join(format!("{name_or_path}.md"));
    if global.exists() {
        return parse_agent_file(&global);
    }

    // 4. Built-in agents (lowest priority — fallback)
    for agent in builtin_agents() {
        if agent.name == name_or_path {
            return Some(agent);
        }
    }

    None
}

/// Return the description of the agent with the given name, if found.
pub fn agent_description(name: &str) -> Option<String> {
    find_agent(name).map(|a| a.description)
}

// ---------------------------------------------------------------------------
// Apply agent config to CLI parameters
// ---------------------------------------------------------------------------

impl AgentConfig {
    /// Apply this agent's settings to overridable parameters.
    pub fn apply(&self, model: &mut String, thinking: &mut Option<String>, max_turns: &mut Option<u64>, prompt: &mut Option<String>) {
        if let Some(ref m) = self.model { *model = m.clone(); }
        if let Some(ref tl) = self.thinking_level { *thinking = Some(tl.clone()); }
        if let Some(mt) = self.max_turns { *max_turns = Some(mt); }
        if let Some(ref sp) = self.system_prompt { *prompt = Some(sp.clone()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_builtin_agents() {
        let count = count_builtin_agents();
        // build, explore, plan, improver — at least 3
        assert!(count >= 3, "Expected at least 3 builtin agents, got {}", count);
    }

    #[test]
    fn test_agent_description() {
        // The builtin "build" agent should have a non-empty description
        let desc = agent_description("build");
        assert!(desc.is_some(), "agent_description('build') should return Some");
        assert!(!desc.unwrap().is_empty(), "build agent description should not be empty");

        // A non-existent agent should return None
        let missing = agent_description("nonexistent_agent_xyz_123");
        assert!(missing.is_none(), "agent_description for nonexistent agent should return None");
    }
}
