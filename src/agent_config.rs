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

pub fn builtin_agents() -> Vec<AgentConfig> {
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
            source: "builtin".into(),
        },
    ]
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
    std::env::current_dir().ok().map(|p| p.join(".ion").join("agents"))
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
    config.source = if file_path.contains(".ion/agents/") { "project".into() } else { "user".into() };
    Some(config)
}

// ---------------------------------------------------------------------------
// Find agent by name or path
// ---------------------------------------------------------------------------

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
