//! Workflow DSL — workflow.yaml 解析 + 校验
//!
//! 定义 workflow 的 YAML 格式：stages 列表 + context + defaults。
//! 每个 stage 有 agent/task/gate/if/on_fail/cleanup/outputs 字段。
//!
//! 用法见 docs/design/WORKFLOW_ENGINE.md

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 顶层结构
// ---------------------------------------------------------------------------

/// 完整的 workflow 定义（从 YAML 解析）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowConfig {
    pub name: String,
    #[serde(default)]
    pub context: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    pub stages: Vec<WorkflowStage>,
}

/// 全局默认值
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowDefaults {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_max_loops")]
    pub max_loops: u32,
    #[serde(default = "default_true")]
    pub cleanup_on_success: bool,
    #[serde(default = "default_false")]
    pub cleanup_on_failure: bool,
}

impl Default for WorkflowDefaults {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_loops: 3,
            cleanup_on_success: true,
            cleanup_on_failure: false,
        }
    }
}

fn default_max_retries() -> u32 { 3 }
fn default_max_loops() -> u32 { 3 }
fn default_true() -> bool { true }
fn default_false() -> bool { false }

// ---------------------------------------------------------------------------
// Stage
// ---------------------------------------------------------------------------

/// 单个 stage 定义
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStage {
    /// 唯一标识（被 if/loop_back 引用）
    pub id: String,
    /// agent 名称（与 commands 二选一）
    #[serde(default)]
    pub agent: Option<String>,
    /// 任务描述（支持 {{context.xxx}} 模板变量）
    #[serde(default)]
    pub task: Option<String>,
    /// 直接执行的 bash 命令（与 agent 二选一）
    #[serde(default)]
    pub commands: Option<Vec<String>>,
    /// git worktree 隔离
    #[serde(default)]
    pub worktree: bool,
    /// 条件表达式
    #[serde(default)]
    pub r#if: Option<String>,
    /// 输出映射
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    /// gate 校验
    #[serde(default)]
    pub gate: Option<WorkflowGate>,
    /// 失败处理
    #[serde(default)]
    pub on_fail: Option<WorkflowOnFail>,
    /// worktree 清理
    #[serde(default)]
    pub cleanup: Option<WorkflowCleanup>,
    /// 运行时状态（引擎写入，用户不填）
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_status() -> String { "pending".into() }

/// Gate 校验
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowGate {
    pub command: String,
    #[serde(default = "default_expected")]
    pub expected: String,
    #[serde(default)]
    pub max_retries: Option<u32>,
}

fn default_expected() -> String { "PASS".into() }

/// 失败处理
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowOnFail {
    pub loop_back: String,
    #[serde(default)]
    pub max_loops: Option<u32>,
}

/// worktree 清理策略
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowCleanup {
    #[serde(default)]
    pub on_success: Option<bool>,
    #[serde(default)]
    pub on_failure: Option<bool>,
}

// ---------------------------------------------------------------------------
// 解析 + 校验
// ---------------------------------------------------------------------------

impl WorkflowConfig {
    /// 从 YAML 字符串解析
    pub fn parse(yaml: &str) -> Result<Self, WorkflowError> {
        let config: WorkflowConfig = serde_yaml::from_str(yaml)
            .map_err(|e| WorkflowError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// 从文件解析
    pub fn load(path: &str) -> Result<Self, WorkflowError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| WorkflowError::Io(format!("cannot read {}: {}", path, e)))?;
        Self::parse(&content)
    }

    /// 校验 workflow 定义
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.stages.is_empty() {
            return Err(WorkflowError::Invalid("workflow has no stages".into()));
        }

        // 收集所有 stage id
        let ids: Vec<&str> = self.stages.iter().map(|s| s.id.as_str()).collect();

        for stage in &self.stages {
            // 必须有 agent 或 commands（二选一）
            let has_agent = stage.agent.is_some();
            let has_commands = stage.commands.is_some();
            if !has_agent && !has_commands {
                return Err(WorkflowError::Invalid(format!(
                    "stage '{}' missing required field: agent or commands", stage.id
                )));
            }
            if has_agent && has_commands {
                return Err(WorkflowError::Invalid(format!(
                    "stage '{}' has both agent and commands (mutually exclusive)", stage.id
                )));
            }
            // agent 必须有 task
            if has_agent && stage.task.is_none() {
                return Err(WorkflowError::Invalid(format!(
                    "stage '{}' has agent but no task", stage.id
                )));
            }
            // loop_back 指向的 stage 必须存在
            if let Some(ref on_fail) = stage.on_fail {
                if !ids.contains(&on_fail.loop_back.as_str()) {
                    return Err(WorkflowError::Invalid(format!(
                        "stage '{}' on_fail.loop_back='{}' but no stage with id '{}' exists",
                        stage.id, on_fail.loop_back, on_fail.loop_back
                    )));
                }
            }
            // stage id 唯一
            let dup_count = ids.iter().filter(|&&id| id == stage.id).count();
            if dup_count > 1 {
                return Err(WorkflowError::Invalid(format!(
                    "duplicate stage id: '{}'", stage.id
                )));
            }
        }
        Ok(())
    }

    /// 找第一个非 done 的 stage（用于断点恢复）
    pub fn next_pending_stage(&self) -> Option<&WorkflowStage> {
        self.stages.iter().find(|s| s.status == "pending" || s.status == "failed")
    }

    /// 所有 stage 都 done？
    pub fn is_complete(&self) -> bool {
        self.stages.iter().all(|s| s.status == "done" || s.status == "skipped")
    }

    /// 获取 stage 的有效 max_retries（stage > defaults）
    pub fn effective_max_retries(&self, stage: &WorkflowStage) -> u32 {
        stage.gate.as_ref()
            .and_then(|g| g.max_retries)
            .unwrap_or(self.defaults.max_retries)
    }

    /// 获取 stage 的有效 max_loops
    pub fn effective_max_loops(&self, stage: &WorkflowStage) -> u32 {
        stage.on_fail.as_ref()
            .and_then(|f| f.max_loops)
            .unwrap_or(self.defaults.max_loops)
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum WorkflowError {
    Parse(String),
    Io(String),
    Invalid(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::Parse(msg) => write!(f, "YAML parse error: {}", msg),
            WorkflowError::Io(msg) => write!(f, "IO error: {}", msg),
            WorkflowError::Invalid(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for WorkflowError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_YAML: &str = r#"
name: test
context:
  project: "my-app"
stages:
  - id: develop
    agent: developer
    task: "create hello.py"
    gate:
      command: "ls hello.py && echo EXISTS"
      expected: EXISTS
    on_fail:
      loop_back: develop
      max_loops: 3
    cleanup:
      on_success: true
"#;

    #[test]
    fn test_parse_valid() {
        let wf = WorkflowConfig::parse(VALID_YAML).unwrap();
        assert_eq!(wf.name, "test");
        assert_eq!(wf.stages.len(), 1);
        assert_eq!(wf.stages[0].id, "develop");
        assert_eq!(wf.stages[0].agent.as_deref(), Some("developer"));
        assert!(wf.stages[0].worktree == false);
    }

    #[test]
    fn test_parse_context() {
        let wf = WorkflowConfig::parse(VALID_YAML).unwrap();
        assert_eq!(wf.context.get("project").unwrap(), &serde_json::Value::String("my-app".into()));
    }

    #[test]
    fn test_validate_missing_agent_and_commands() {
        let yaml = r#"
name: bad
stages:
  - id: x
"#;
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing required field"));
    }

    #[test]
    fn test_validate_agent_and_commands_mutually_exclusive() {
        let yaml = r#"
name: bad
stages:
  - id: x
    agent: developer
    task: test
    commands: ["echo hi"]
"#;
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mutually exclusive"));
    }

    #[test]
    fn test_validate_bad_loop_back() {
        let yaml = r#"
name: bad
stages:
  - id: x
    agent: developer
    task: test
    on_fail:
      loop_back: nonexistent
"#;
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn test_validate_agent_without_task() {
        let yaml = r#"
name: bad
stages:
  - id: x
    agent: developer
"#;
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no task"));
    }

    #[test]
    fn test_validate_empty_stages() {
        let yaml = "name: empty\nstages: []";
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no stages"));
    }

    #[test]
    fn test_validate_duplicate_id() {
        let yaml = r#"
name: dup
stages:
  - id: x
    agent: developer
    task: a
  - id: x
    agent: developer
    task: b
"#;
        let result = WorkflowConfig::parse(yaml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn test_next_pending_stage() {
        let mut wf = WorkflowConfig::parse(VALID_YAML).unwrap();
        assert!(wf.next_pending_stage().is_some()); // develop is pending
        wf.stages[0].status = "done".into();
        assert!(wf.next_pending_stage().is_none()); // all done
    }

    #[test]
    fn test_is_complete() {
        let mut wf = WorkflowConfig::parse(VALID_YAML).unwrap();
        assert!(!wf.is_complete());
        wf.stages[0].status = "done".into();
        assert!(wf.is_complete());
    }

    #[test]
    fn test_effective_max_retries() {
        let wf = WorkflowConfig::parse(VALID_YAML).unwrap();
        // stage 没设 max_retries → 用 defaults (3)
        assert_eq!(wf.effective_max_retries(&wf.stages[0]), 3);
    }

    #[test]
    fn test_commands_only_stage() {
        let yaml = r#"
name: cmds
stages:
  - id: cleanup
    commands: ["echo hello", "echo world"]
"#;
        let wf = WorkflowConfig::parse(yaml).unwrap();
        assert!(wf.stages[0].agent.is_none());
        assert_eq!(wf.stages[0].commands.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_defaults() {
        let yaml = r#"
name: d
defaults:
  max_retries: 5
  max_loops: 2
stages:
  - id: x
    agent: dev
    task: t
"#;
        let wf = WorkflowConfig::parse(yaml).unwrap();
        assert_eq!(wf.defaults.max_retries, 5);
        assert_eq!(wf.defaults.max_loops, 2);
        assert_eq!(wf.defaults.cleanup_on_success, true);
        assert_eq!(wf.defaults.cleanup_on_failure, false);
    }
}
