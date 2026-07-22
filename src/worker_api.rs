use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// BridgeHandle — 让 ExtensionApi 通过 JSON stdout 协议与 Manager 通信。
///
/// 由 ion_worker 中的 ManagerBridge 实现，替代 dead mpsc ManagerCommand 路径。
#[async_trait]
pub trait BridgeHandle: Send + Sync {
    /// 发送一个 manager_command 到 Manager，等待响应（阻塞等待）。
    async fn send_command(&self, command: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;
}

/// Worker creation config for extensions.
///
/// 与 Manager 端 `WorkerCreateConfig` 对齐，字段透传。
/// 补丁 1（HOOKS_AND_OUTLINE_SYNC）补齐了 agent/initial_prompt/worktree/allowed_tools/max_turns
/// 等字段——这是 hooks 的 agent handler 能"真调工具"的前提。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ExtensionWorkerConfig {
    pub session: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub channels: Option<Vec<String>>,
    pub parent: Option<String>,

    // ── 补丁 1 新增字段（与 WorkerCreateConfig 对齐）──
    /// Agent 角色（对应 .ion/agents/<name>.md）。设了子 Worker 才有 agent 定义。
    pub agent: Option<String>,
    /// 子 Worker 的初始 prompt（任务描述）。由内核通过 prompt RPC 发给子进程。
    pub initial_prompt: Option<String>,
    /// Worktree 隔离配置。Some → 在独立 git worktree 里跑。
    pub worktree: Option<crate::worker_registry::WorktreeConfig>,
    /// 与创建者的关系。默认 Child。
    pub relation: Option<String>,
    /// 允许的工具白名单（None = 继承全部）。子 Worker 启动时通过 ION_ALLOWED_TOOLS 环境变量传入。
    pub allowed_tools: Option<Vec<String>>,
    /// 禁用的工具黑名单。通过 ION_DISALLOWED_TOOLS 环境变量传入。
    pub disallowed_tools: Option<Vec<String>>,
    /// 最大 turn 数（None = 继承 host 默认）。通过 ION_MAX_TURNS 环境变量传入。
    pub max_turns: Option<u64>,
}

/// Worker info returned after creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionWorkerInfo {
    pub worker_id: String,
    pub session_id: String,
}

/// A handle to a Worker, all methods go through JSONL RPC to Manager.
pub struct WorkerHandle {
    pub worker_id: String,
    pub session_id: String,
    /// Channel to send commands to Manager (which routes to Worker stdin)
    manager_tx: mpsc::Sender<ManagerCommand>,
}

impl WorkerHandle {
    pub fn new(worker_id: String, session_id: String, manager_tx: mpsc::Sender<ManagerCommand>) -> Self {
        Self { worker_id, session_id, manager_tx }
    }

    pub fn id(&self) -> &str { &self.worker_id }
    pub fn session_id(&self) -> &str { &self.session_id }

    /// Send a prompt to this Worker.
    pub async fn send(&self, text: impl Into<String>) -> Result<(), String> {
        self.manager_tx.send(ManagerCommand::SendToWorker {
            worker_id: self.worker_id.clone(),
            method: "prompt".into(),
            params: serde_json::json!({"text": text.into()}),
        }).await.map_err(|e| e.to_string())
    }

    /// Send an RPC command to this Worker.
    pub async fn rpc(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        self.manager_tx.send(ManagerCommand::SendToWorker {
            worker_id: self.worker_id.clone(),
            method: method.into(),
            params,
        }).await.map_err(|e| e.to_string())
    }

    /// Steer this Worker.
    pub async fn steer(&self, text: impl Into<String>) -> Result<(), String> {
        self.rpc("steer", serde_json::json!({"text": text.into()})).await
    }

    /// Kill this Worker.
    pub async fn kill(&self) -> Result<(), String> {
        self.manager_tx.send(ManagerCommand::KillWorker {
            worker_id: self.worker_id.clone(),
        }).await.map_err(|e| e.to_string())
    }

    /// Send to a channel.
    pub async fn channel_send(&self, channel: &str, msg: serde_json::Value) -> Result<(), String> {
        self.manager_tx.send(ManagerCommand::ChannelSend {
            channel: channel.into(),
            from: self.worker_id.clone(),
            msg,
        }).await.map_err(|e| e.to_string())
    }
}

impl Clone for WorkerHandle {
    fn clone(&self) -> Self {
        Self {
            worker_id: self.worker_id.clone(),
            session_id: self.session_id.clone(),
            manager_tx: self.manager_tx.clone(),
        }
    }
}

/// Commands sent from WorkerHandle to Manager.
#[derive(Debug)]
pub enum ManagerCommand {
    SendToWorker {
        worker_id: String,
        method: String,
        params: serde_json::Value,
    },
    KillWorker {
        worker_id: String,
    },
    ChannelSend {
        channel: String,
        from: String,
        msg: serde_json::Value,
    },
    CreateWorker {
        config: ExtensionWorkerConfig,
        reply: tokio::sync::oneshot::Sender<ExtensionWorkerInfo>,
    },
}

/// ExtensionApi — the API available to plugins running inside a Worker.
///
/// Extensions use this to:
/// - Create child Workers
/// - Send messages to other Workers
/// - Subscribe to events
/// - Broadcast to channels
pub struct ExtensionApi {
    /// This Worker's ID
    pub worker_id: String,
    /// This Worker's session ID
    pub session_id: String,
    /// Channel to send commands to Manager
    pub manager_tx: mpsc::Sender<ManagerCommand>,
    /// Channel to inject follow_up messages back into the current agent
    /// (used by bash extension for background process completion notification)
    pub follow_up_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::agent::messages::Message>>,
    /// Bridge to Manager via JSON stdout protocol.
    /// When set, create_worker() uses this instead of the dead mpsc path.
    pub bridge: Option<Arc<dyn BridgeHandle>>,
}

impl ExtensionApi {
    pub fn new(worker_id: String, session_id: String, manager_tx: mpsc::Sender<ManagerCommand>) -> Self {
        Self { worker_id, session_id, manager_tx, follow_up_tx: None, bridge: None }
    }

    /// Set the bridge to Manager (JSON stdout protocol).
    /// When set, create_worker() uses this instead of the dead mpsc path.
    pub fn with_bridge(mut self, bridge: Arc<dyn BridgeHandle>) -> Self {
        self.bridge = Some(bridge);
        self
    }

    /// Set the follow_up channel sender.
    /// Called by the worker during startup, before the extension is used.
    pub fn set_follow_up_tx(&mut self, tx: tokio::sync::mpsc::UnboundedSender<crate::agent::messages::Message>) {
        self.follow_up_tx = Some(tx);
    }

    /// Inject a message into the agent's follow_up queue.
    /// Used by background process completion, e.g. bash extension.
    /// Messages will be processed after the current command completes.
    pub fn inject_follow_up(&self, msg: crate::agent::messages::Message) {
        if let Some(ref tx) = self.follow_up_tx {
            let _ = tx.send(msg);
        }
    }

    /// Create a child Worker. Returns a WorkerHandle.
    /// Uses BridgeHandle when available, falls back to mpsc (dead code path).
    pub async fn create_worker(&self, config: ExtensionWorkerConfig) -> Result<WorkerHandle, String> {
        if let Some(ref bridge) = self.bridge {
            // Live path: JSON stdout → Manager
            // 透传所有字段（含补丁 1 新增的 agent/initial_prompt/worktree/allowed_tools/max_turns）
            // Manager 端 WorkerCreateConfig 用 serde_json::from_value 反序列化，字段自动消费
            let params = serde_json::json!({
                "session": config.session,
                "model": config.model,
                "provider": config.provider,
                "channels": config.channels,
                "parent": config.parent,
                // ── 补丁 1 新增透传 ──
                "agent": config.agent,
                "initial_prompt": config.initial_prompt,
                "worktree": config.worktree,
                "relation": config.relation,
                "allowed_tools": config.allowed_tools,
                "disallowed_tools": config.disallowed_tools,
                "max_turns": config.max_turns,
            });
            let resp = bridge.send_command("create_worker", params).await?;
            let worker_id = resp.get("workerId").and_then(|v| v.as_str()).ok_or("create_worker: missing workerId in response")?.to_string();
            let session_id = resp.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Ok(WorkerHandle::new(worker_id, session_id.clone(), self.manager_tx.clone()))
        } else {
            // Dead code path: mpsc ManagerCommand (no receiver exists!)
            let (reply, rx) = tokio::sync::oneshot::channel();
            self.manager_tx.send(ManagerCommand::CreateWorker {
                config, reply,
            }).await.map_err(|e| e.to_string())?;
            let info = rx.await.map_err(|e| e.to_string())?;
            Ok(WorkerHandle::new(info.worker_id, info.session_id, self.manager_tx.clone()))
        }
    }

    /// Get a handle to self.
    pub fn self_handle(&self) -> WorkerHandle {
        WorkerHandle::new(self.worker_id.clone(), self.session_id.clone(), self.manager_tx.clone())
    }

    /// Get a handle to an existing Worker by ID.
    pub fn get_worker(&self, worker_id: &str) -> WorkerHandle {
        WorkerHandle::new(worker_id.to_string(), String::new(), self.manager_tx.clone())
    }

    /// Send to a channel.
    pub async fn channel_send(&self, channel: &str, msg: serde_json::Value) -> Result<(), String> {
        self.self_handle().channel_send(channel, msg).await
    }

    /// Emit a custom event (goes to stdout → Manager → subscribers).
    pub fn emit(&self, event_type: &str, data: serde_json::Value) {
        let msg = serde_json::json!({
            "type": "event",
            "event": {
                "type": "custom",
                "customType": event_type,
                "data": data,
            }
        });
        println!("{}", serde_json::to_string(&msg).unwrap_or_default());
    }

    /// 发射一条插件事件（通过 stdout → Manager EventBus → subscriber）。
    /// 插件只管 emit，不碰传输层。
    pub fn emit_extension_event(&self, event: crate::event_bus::ExtensionEvent) {
        let msg = serde_json::json!({
            "type": "extension_event",
            "extension": event.extension,
            "customType": event.custom_type,
            "session": event.session,
            "visibility": match event.visibility {
                crate::event_bus::EventVisibility::LlmAndUi => "llm_and_ui",
                crate::event_bus::EventVisibility::UiOnly => "ui_only",
            },
            "correlation_id": event.correlation_id,
            "persisted": event.persisted,
            "data": event.data,
        });
        println!("{}", serde_json::to_string(&msg).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::{EventVisibility, ExtensionEvent};

    // ── ExtensionWorkerConfig ──

    #[test]
    fn extension_worker_config_default_is_all_none() {
        // Default construction should leave every optional field as None,
        // matching Manager-side expectations when nothing is specified.
        let cfg = ExtensionWorkerConfig::default();
        assert!(cfg.session.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.provider.is_none());
        assert!(cfg.channels.is_none());
        assert!(cfg.parent.is_none());
        assert!(cfg.agent.is_none());
        assert!(cfg.initial_prompt.is_none());
        assert!(cfg.worktree.is_none());
        assert!(cfg.relation.is_none());
        assert!(cfg.allowed_tools.is_none());
        assert!(cfg.disallowed_tools.is_none());
        assert!(cfg.max_turns.is_none());
    }

    #[test]
    fn extension_worker_config_round_trips_through_json() {
        // The struct must serialize/deserialize losslessly because the Manager
        // reconstructs it via serde_json::from_value on the receiving side.
        let cfg = ExtensionWorkerConfig {
            session: Some("sess-1".into()),
            model: Some("gpt-4".into()),
            provider: Some("openai".into()),
            channels: Some(vec!["ch-a".into(), "ch-b".into()]),
            parent: Some("parent-1".into()),
            agent: Some("coder".into()),
            initial_prompt: Some("do the thing".into()),
            worktree: None,
            relation: Some("Child".into()),
            allowed_tools: Some(vec!["bash".into()]),
            disallowed_tools: Some(vec!["rm".into()]),
            max_turns: Some(42),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ExtensionWorkerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session.as_deref(), Some("sess-1"));
        assert_eq!(back.model.as_deref(), Some("gpt-4"));
        assert_eq!(back.provider.as_deref(), Some("openai"));
        assert_eq!(back.channels.as_deref(), Some(&["ch-a".to_string(), "ch-b".to_string()][..]));
        assert_eq!(back.parent.as_deref(), Some("parent-1"));
        assert_eq!(back.agent.as_deref(), Some("coder"));
        assert_eq!(back.initial_prompt.as_deref(), Some("do the thing"));
        assert!(back.worktree.is_none());
        assert_eq!(back.relation.as_deref(), Some("Child"));
        assert_eq!(back.allowed_tools.as_deref(), Some(&["bash".to_string()][..]));
        assert_eq!(back.disallowed_tools.as_deref(), Some(&["rm".to_string()][..]));
        assert_eq!(back.max_turns, Some(42));
    }

    #[test]
    fn extension_worker_config_serializes_to_object() {
        // The JSON shape must be an object (serde_json::Value::Object) — the
        // create_worker code in ExtensionApi merges these keys into an RPC payload.
        let cfg = ExtensionWorkerConfig {
            session: Some("s".into()),
            ..Default::default()
        };
        let value: serde_json::Value =
            serde_json::to_value(&cfg).expect("serialize failed");
        assert!(value.is_object());
        assert_eq!(value.get("session").and_then(|v| v.as_str()), Some("s"));
    }

    // ── ExtensionWorkerInfo ──

    #[test]
    fn extension_worker_info_round_trips_through_json() {
        // WorkerInfo is the reply payload returned from Manager on worker creation;
        // it must deserialize back into the same struct.
        let info = ExtensionWorkerInfo {
            worker_id: "w-1".into(),
            session_id: "s-1".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: ExtensionWorkerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.worker_id, "w-1");
        assert_eq!(back.session_id, "s-1");
    }

    #[test]
    fn extension_worker_info_rejects_missing_fields() {
        // Deserialization should fail when a required field is absent.
        let bad = serde_json::json!({"worker_id": "w-1"});
        let res: Result<ExtensionWorkerInfo, _> = serde_json::from_value(bad);
        assert!(res.is_err());
    }

    // ── WorkerHandle (sync accessors only) ──

    #[test]
    fn worker_handle_new_stores_ids() {
        // The sync constructor + accessors must round-trip the id/session_id.
        let (tx, _rx) = mpsc::channel::<ManagerCommand>(1);
        let handle = WorkerHandle::new("worker-1".into(), "session-1".into(), tx);
        assert_eq!(handle.id(), "worker-1");
        assert_eq!(handle.session_id(), "session-1");
    }

    #[test]
    fn worker_handle_clone_preserves_ids() {
        // Cloning a WorkerHandle must copy the worker_id and session_id.
        let (tx, _rx) = mpsc::channel::<ManagerCommand>(1);
        let handle = WorkerHandle::new("worker-2".into(), "session-2".into(), tx);
        let cloned = handle.clone();
        assert_eq!(cloned.id(), "worker-2");
        assert_eq!(cloned.session_id(), "session-2");
    }

    // ── ManagerCommand variants ──

    #[test]
    fn manager_command_kill_worker_construction() {
        // Pure construction check: KillWorker should carry the worker_id verbatim.
        let cmd = ManagerCommand::KillWorker { worker_id: "w-kill".into() };
        match cmd {
            ManagerCommand::KillWorker { worker_id } => assert_eq!(worker_id, "w-kill"),
            _ => panic!("expected KillWorker variant"),
        }
    }

    #[test]
    fn manager_command_channel_send_construction() {
        // Pure construction check: ChannelSend carries channel/from/msg fields.
        let cmd = ManagerCommand::ChannelSend {
            channel: "ch".into(),
            from: "sender".into(),
            msg: serde_json::json!({"hello": "world"}),
        };
        match cmd {
            ManagerCommand::ChannelSend { channel, from, msg } => {
                assert_eq!(channel, "ch");
                assert_eq!(from, "sender");
                assert_eq!(msg.get("hello").and_then(|v| v.as_str()), Some("world"));
            }
            _ => panic!("expected ChannelSend variant"),
        }
    }

    // ── ExtensionApi sync accessors ──

    #[test]
    fn extension_api_new_defaults_bridge_and_follow_up_to_none() {
        // The base constructor should leave bridge and follow_up_tx unset.
        let (tx, _rx) = mpsc::channel::<ManagerCommand>(1);
        let api = ExtensionApi::new("w-self".into(), "s-self".into(), tx);
        assert_eq!(api.worker_id, "w-self");
        assert_eq!(api.session_id, "s-self");
        assert!(api.bridge.is_none());
        assert!(api.follow_up_tx.is_none());
    }

    #[test]
    fn extension_api_self_handle_copies_ids() {
        // self_handle() must build a WorkerHandle whose ids match the api.
        let (tx, _rx) = mpsc::channel::<ManagerCommand>(1);
        let api = ExtensionApi::new("w-self".into(), "s-self".into(), tx);
        let h = api.self_handle();
        assert_eq!(h.id(), "w-self");
        assert_eq!(h.session_id(), "s-self");
    }

    #[test]
    fn extension_api_get_worker_yields_given_id() {
        // get_worker must preserve the caller-supplied worker_id (session left empty).
        let (tx, _rx) = mpsc::channel::<ManagerCommand>(1);
        let api = ExtensionApi::new("w-self".into(), "s-self".into(), tx);
        let h = api.get_worker("other-worker");
        assert_eq!(h.id(), "other-worker");
        assert_eq!(h.session_id(), "");
    }

    // ── emit_extension_event visibility mapping (pure logic, no I/O assertions) ──

    #[test]
    fn extension_event_visibility_default_is_ui_only() {
        // The default ExtensionEvent starts as UiOnly per the constructor.
        let ev = ExtensionEvent::new("memory", "saved");
        assert_eq!(ev.visibility, EventVisibility::UiOnly);
    }

    #[test]
    fn extension_event_visibility_can_be_upgraded_to_llm_and_ui() {
        // Builder should allow flipping visibility — used by emit_extension_event
        // when serializing the "llm_and_ui" string.
        let ev = ExtensionEvent::new("memory", "saved")
            .with_visibility(EventVisibility::LlmAndUi);
        assert_eq!(ev.visibility, EventVisibility::LlmAndUi);
    }
}
