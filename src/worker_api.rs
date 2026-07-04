use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Worker creation config for plugins.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PluginWorkerConfig {
    pub session: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub channels: Option<Vec<String>>,
    pub parent: Option<String>,
}

/// Worker info returned after creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginWorkerInfo {
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
        config: PluginWorkerConfig,
        reply: tokio::sync::oneshot::Sender<PluginWorkerInfo>,
    },
}

/// ExtensionApi — the API available to plugins running inside a Worker.
///
/// Plugins use this to:
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
}

impl ExtensionApi {
    pub fn new(worker_id: String, session_id: String, manager_tx: mpsc::Sender<ManagerCommand>) -> Self {
        Self { worker_id, session_id, manager_tx }
    }

    /// Create a child Worker. Returns a WorkerHandle.
    pub async fn create_worker(&self, config: PluginWorkerConfig) -> Result<WorkerHandle, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.manager_tx.send(ManagerCommand::CreateWorker {
            config, reply,
        }).await.map_err(|e| e.to_string())?;
        let info = rx.await.map_err(|e| e.to_string())?;
        Ok(WorkerHandle::new(info.worker_id, info.session_id, self.manager_tx.clone()))
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
    pub fn emit_plugin_event(&self, event: crate::event_bus::PluginEvent) {
        let msg = serde_json::json!({
            "type": "plugin_event",
            "plugin": event.plugin,
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
