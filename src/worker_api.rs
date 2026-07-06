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

/// Worker creation config for plugins.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ExtensionWorkerConfig {
    pub session: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub channels: Option<Vec<String>>,
    pub parent: Option<String>,
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
            let params = serde_json::json!({
                "session": config.session,
                "model": config.model,
                "provider": config.provider,
                "channels": config.channels,
                "parent": config.parent,
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
