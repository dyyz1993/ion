//! GlobalMemoryExtension — 跨项目记忆单例扩展。
//!
//! 在 ion serve 启动时注册为单例（singleton_key = "global-memory"）。
//! 所有 Worker 共享同一份全局记忆库。
//! 通过 extension_rpc 提供 save/search/list/forget 接口。
//! Memory Agent Worker（Phase 7）通过这些接口检索记忆。

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::extension::Extension;
use crate::global_memory::{GlobalMemoryEntry, GlobalMemoryStore};
use async_trait::async_trait;
use std::sync::Arc;

/// 全局记忆单例扩展
pub struct GlobalMemoryExtension {
    /// 全局记忆库（on_singleton_init 时打开）
    store: Arc<std::sync::Mutex<Option<GlobalMemoryStore>>>,
    /// Active Memory sub-agent 的 worker_id（post_init 时 spawn，shutdown 时 kill）
    active_memory_worker: std::sync::Mutex<Option<String>>,
}

impl GlobalMemoryExtension {
    pub fn new() -> Self {
        Self {
            store: Arc::new(std::sync::Mutex::new(None)),
            active_memory_worker: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl Extension for GlobalMemoryExtension {
    fn name(&self) -> &str {
        "global-memory"
    }

    fn is_singleton(&self) -> bool {
        true
    }

    fn singleton_key(&self) -> &str {
        "global-memory"
    }

    async fn on_singleton_init(&self) -> AgentResult<()> {
        let db_path = GlobalMemoryStore::db_path();
        tracing::info!("[global-memory] opening db at {}", db_path.display());
        let store = GlobalMemoryStore::open(&db_path)
            .map_err(|e| AgentError::Tool(format!("global memory db: {}", e)))?;
        // V0.1 → V0.2 自动迁移（DB 空时执行）
        match store.migrate_from_v01() {
            Ok(n) if n > 0 => tracing::info!("[global-memory] migrated {} entries from V0.1", n),
            Ok(_) => {}
            Err(e) => tracing::warn!("[global-memory] migration failed: {}", e),
        }
        let mut guard = self.store.lock().unwrap();
        *guard = Some(store);
        tracing::info!("[global-memory] db opened, singleton initialized");
        Ok(())
    }

    /// post_init：spawn Active Memory sub-agent（系统级 Worker）
    ///
    /// 在 init_singletons 释放 lock 后调用（post_init 内部 lock registry 来 create_worker）。
    /// memory-agent 是一个带 LLM 的常驻 Worker，其他 Worker 通过 send_to_worker 查询它。
    async fn on_singleton_post_init(
        &self,
        registry: &std::sync::Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>,
    ) -> AgentResult<()> {
        // 读 config 拿 model/provider（memory-agent 用默认模型）
        let cfg = crate::config::IonConfig::load();
        // 如果 global-memory 在 config 里被显式禁用，跳过 memory-agent spawn（避免无谓消耗 LLM 额度）
        if !cfg.is_extension_enabled("global-memory") {
            tracing::info!("[global-memory] disabled by config, skipping memory-agent spawn");
            return Ok(());
        }
        let model = cfg.default_model.clone().unwrap_or_else(|| "deepseek-v4-flash".into());
        let provider = cfg.default_provider.clone().unwrap_or_else(|| "opencode".into());

        // 构造 memory-agent 的 WorkerCreateConfig
        let config = crate::worker_registry::WorkerCreateConfig {
            session: Some(format!("sess_memory_agent_{}", &uuid::Uuid::new_v4().to_string()[..8])),
            agent: Some("memory-agent".into()),
            model: Some(model),
            provider: Some(provider),
            relation: Some(crate::worker_registry::WorkerRelation::System),
            parent: None,
            initial_prompt: Some(
                "You are the Memory Agent. Waiting for queries from other Workers. \
                 Use global_memory_search to find relevant memories when asked."
                    .into(),
            ),
            channels: Some(vec!["main".into()]),
            ..Default::default()
        };

        tracing::info!("[global-memory] spawning Active Memory sub-agent...");
        let mut reg = registry.lock().await;
        match reg.create_worker(config, registry).await {
            Ok(info) => {
                let wid = info.worker_id.clone();
                tracing::info!("[global-memory] Active Memory sub-agent started: {} (session: {})", wid, info.session_id);
                *self.active_memory_worker.lock().unwrap() = Some(wid);
            }
            Err(e) => {
                tracing::error!("[global-memory] failed to spawn memory-agent: {}", e);
                // 不报错（memory-agent 是可选增强，spawn 失败不应阻断 host 启动）
            }
        }
        Ok(())
    }

    async fn on_singleton_shutdown(&self) -> AgentResult<()> {
        tracing::info!("[global-memory] singleton shutting down");
        // kill memory-agent Worker（如果存在）
        if let Some(wid) = self.active_memory_worker.lock().unwrap().take() {
            tracing::info!("[global-memory] killing memory-agent: {}", wid);
            // 注意：kill 需要 registry，但 shutdown 时可能拿不到。
            // Worker 进程会在 host 退出时自然终止（child process）。
            // 这里只清理记录。如果需要显式 kill，可在 host shutdown 流程里加。
        }
        let mut guard = self.store.lock().unwrap();
        *guard = None;
        Ok(())
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> AgentResult<serde_json::Value> {
        let guard = self.store.lock().unwrap();
        let store = guard.as_ref().ok_or_else(|| {
            AgentError::Tool("global memory not initialized (serve mode only)".into())
        })?;

        match method {
            "save" => {
                let content = params.get("content").and_then(|v| v.as_str())
                    .ok_or_else(|| AgentError::Tool("missing 'content'".into()))?;
                let category = params.get("category").and_then(|v| v.as_str()).unwrap_or("");
                let tags = params.get("tags").and_then(|v| v.as_str()).unwrap_or("");
                let project = params.get("project").and_then(|v| v.as_str())
                    .ok_or_else(|| AgentError::Tool("missing 'project'".into()))?;
                let importance = params.get("importance").and_then(|v| v.as_i64())
                    .unwrap_or(5) as i32;
                let id = store.save(content, category, tags, project, importance)
                    .map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"id": id}))
            }

            "search" => {
                let query = params.get("query").and_then(|v| v.as_str())
                    .ok_or_else(|| AgentError::Tool("missing 'query'".into()))?;
                let project = params.get("project").and_then(|v| v.as_str());
                let results = store.search(query, project)
                    .map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"results": serialize_entries(&results)}))
            }

            "list" => {
                let project = params.get("project").and_then(|v| v.as_str());
                let results = store.list(project)
                    .map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"entries": serialize_entries(&results)}))
            }

            "forget" => {
                let id = params.get("id").and_then(|v| v.as_str())
                    .ok_or_else(|| AgentError::Tool("missing 'id'".into()))?;
                store.forget(id).map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"ok": true}))
            }

            "list_outlines" => {
                let outlines = store.list_outlines()
                    .map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"outlines": outlines}))
            }

            "consolidate" => {
                let stats = store.consolidate()
                    .map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"stats": stats}))
            }

            "clear_stored" => {
                // 清空所有记忆（测试用）——SQL 批量删，不走逐条 forget
                let count = store.count().unwrap_or(0);
                store.clear_all().map_err(|e| AgentError::Tool(e))?;
                Ok(serde_json::json!({"removed": count}))
            }

            _ => Err(AgentError::Tool(format!(
                "unknown method '{}'. Available: save, search, list, forget, list_outlines, consolidate, clear_stored", method
            ))),
        }
    }
}

fn serialize_entries(entries: &[GlobalMemoryEntry]) -> Vec<serde_json::Value> {
    entries.iter().map(|e| serde_json::json!({
        "id": e.id,
        "project": e.project,
        "content": e.content,
        "category": e.category,
        "tags": e.tags,
        "importance": e.importance,
        "created_at": e.created_at,
        "updated_at": e.updated_at,
    })).collect()
}
