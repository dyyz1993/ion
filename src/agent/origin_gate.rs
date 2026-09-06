//! OriginGate — 按 INPUT_ORIGIN 拒绝工具（docs/design/INPUT_ORIGIN.md §2.4 消费侧）。
//!
//! 用途：monitor/system/peer 轮次禁用指定工具（如 spawn_worker、write 等）。
//! 拒绝走 before_tool_call → 错误 ToolResult 闭环（PreToolUse 拒绝链路已验证），
//! agent 继续运行并看到拒绝原因——LLM 仍能看到工具 schema，但执行被拒。
//!
//! 配置（config.json，全局或项目级深度合并后生效）：
//! ```json
//! {
//!   "extensions": { "origin_gate": { "enabled": true } },
//!   "origin_tools": {
//!     "monitor": ["spawn_worker", "write"],
//!     "system":  ["spawn_worker"]
//!   }
//! }
//! ```
//! key = origin 值；value = 该 origin 轮禁用的工具名列表（空/缺省=不限制）。
//! `user` 不允许配置（用户轮不受限，防误锁自己）。

use async_trait::async_trait;
use ion_provider::types::ToolCall;
use std::collections::HashMap;

use super::error::{AgentError, AgentResult};
use super::extension::Extension;
use crate::config::IonConfig;

pub struct OriginGate {
    /// origin → 禁用工具名集合（deny 显式拒绝 + hide 隐藏即拒 合并）
    deny_map: HashMap<String, Vec<String>>,
}

impl OriginGate {
    pub fn from_config(cfg: &IonConfig) -> Self {
        let mut deny_map: HashMap<String, Vec<String>> = cfg
            .origin_tools
            .iter()
            .filter(|(k, _)| k.as_str() != "user") // 用户轮不受限
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // 隐藏即拒：origin_hide_tools 的工具 schema 已不可见，
        // 若 LLM 仍幻觉调用也拒绝执行（防御纵深）
        for (k, list) in cfg.origin_hide_tools.iter() {
            if k == "user" {
                continue;
            }
            deny_map.entry(k.clone()).or_default().extend(list.iter().cloned());
        }
        if !deny_map.is_empty() {
            tracing::info!("[origin_gate] loaded rules: {:?}", deny_map);
        }
        Self { deny_map }
    }

    /// 测试/编程构造
    pub fn from_map(deny_map: HashMap<String, Vec<String>>) -> Self {
        Self { deny_map }
    }
}

#[async_trait]
impl Extension for OriginGate {
    fn name(&self) -> &str {
        "origin_gate"
    }

    async fn before_tool_call_with_origin(
        &self,
        call: &mut ToolCall,
        origin: &str,
    ) -> AgentResult<()> {
        if origin == "user" {
            return Ok(());
        }
        if let Some(denied) = self.deny_map.get(origin) {
            if denied.iter().any(|t| t == &call.name) {
                return Err(AgentError::Tool(format!(
                    "[OriginGate] tool '{}' denied for origin '{origin}' (origin_tools rule)",
                    call.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tool_call(name: &str) -> ToolCall {
        ToolCall {
            call_type: "function".into(),
            id: "tc_test".into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        }
    }

    #[tokio::test]
    async fn monitor_round_denies_configured_tool() {
        let mut map = HashMap::new();
        map.insert("monitor".to_string(), vec!["spawn_worker".to_string()]);
        let gate = OriginGate::from_map(map);

        let mut call = tool_call("spawn_worker");
        let res = gate.before_tool_call_with_origin(&mut call, "monitor").await;
        assert!(res.is_err(), "monitor 轮 spawn_worker 应被拒");
    }

    #[tokio::test]
    async fn monitor_round_allows_other_tools() {
        let mut map = HashMap::new();
        map.insert("monitor".to_string(), vec!["spawn_worker".to_string()]);
        let gate = OriginGate::from_map(map);

        let mut call = tool_call("read");
        let res = gate.before_tool_call_with_origin(&mut call, "monitor").await;
        assert!(res.is_ok(), "monitor 轮 read 不在禁用列表应放行");
    }

    #[tokio::test]
    async fn user_round_never_denied() {
        let mut map = HashMap::new();
        // 即使配置误写了 user 也被 from_config 过滤；from_map 直测防线：
        map.insert("user".to_string(), vec!["write".to_string()]);
        let gate = OriginGate::from_map(map);

        // user 轮直接短路（before_tool_call_with_origin 里 origin=="user" return Ok）
        let mut call = tool_call("write");
        let res = gate.before_tool_call_with_origin(&mut call, "user").await;
        assert!(res.is_ok(), "user 轮不受 origin 限制");
    }

    #[tokio::test]
    async fn system_round_denies() {
        let mut map = HashMap::new();
        map.insert("system".to_string(), vec!["spawn_worker".to_string()]);
        let gate = OriginGate::from_map(map);

        let mut call = tool_call("spawn_worker");
        let res = gate.before_tool_call_with_origin(&mut call, "system").await;
        assert!(res.is_err(), "system 轮 spawn_worker 应被拒");
    }
}
