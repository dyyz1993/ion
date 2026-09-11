//! Host Tools — REMOTE_WORKER M4 通道化宿主访问（worker 侧）。
//!
//! 远程 worker 需要读写 Mac 主控端文件 / 经主控端出网时，不走裸路径/裸网络
//!（路径命名空间=执行侧，Mac 路径对 worker 不可直接达），而是经这三个工具
//! 发 `host_call` 给 Manager：Manager 按 remote_workers.<name>.grants 授权
//! → 执行 → 审计（会话 JSONL host_call custom 条目）→ 回响应。
//! 设计：docs/design/REMOTE_WORKER.md §3.4（封闭动词表，永不提供 host.bash）。

use crate::agent::error::{AgentError, AgentResult};
use crate::agent::tool::Tool;
use std::sync::Arc;

/// 共享的 bridge 句柄（worker_rpc 构造工具时注入）。
pub type HostBridge = Arc<crate::worker_rpc::ManagerBridge>;

async fn host_call(
    bridge: &HostBridge,
    verb: &str,
    args: serde_json::Value,
) -> AgentResult<String> {
    let resp = bridge
        .send_command("host_call", serde_json::json!({"verb": verb, "args": args}))
        .await
        .map_err(AgentError::Tool)?;
    let success = resp
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !success {
        let err = resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown host_call error");
        return Err(AgentError::Tool(format!("{verb}: {err}")));
    }
    // data 形状按 verb 不同：fs.read → {content}; fs.write → {bytes};
    // http.fetch → {status, body}。序列化为紧凑 JSON 交给 LLM。
    let data = resp.get("data").cloned().unwrap_or_default();
    Ok(serde_json::to_string(&data).unwrap_or_default())
}

// ── host_read：读主控端文件（grants.fs_read 授权） ────────────────────────

pub struct HostReadTool(pub HostBridge);

#[async_trait::async_trait]
impl Tool for HostReadTool {
    fn name(&self) -> &str {
        "host_read"
    }
    fn description(&self) -> &str {
        "Read a file on the CONTROLLER host (Mac). Use only for files you are explicitly granted \
         access to (remote_workers.grants.fs_read). Paths are host-side; your own sandbox paths \
         do not apply. Returns {\"content\": \"...\"}."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "path":{"type":"string","description":"Absolute path on the controller host"}
        },"required":["path"]})
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing path".into()))?;
        host_call(&self.0, "fs.read", serde_json::json!({"path": path})).await
    }
}

// ── host_write：写主控端文件（grants.fs_write 授权，谨慎） ────────────────

pub struct HostWriteTool(pub HostBridge);

#[async_trait::async_trait]
impl Tool for HostWriteTool {
    fn name(&self) -> &str {
        "host_write"
    }
    fn description(&self) -> &str {
        "Write a file on the CONTROLLER host (Mac). REQUIRES grants.fs_write — most sessions do \
         not have this. Full-file overwrite semantics. Returns {\"bytes\": N}."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "path":{"type":"string","description":"Absolute path on the controller host"},
            "content":{"type":"string","description":"Full file content"}
        },"required":["path","content"]})
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing path".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing content".into()))?;
        host_call(
            &self.0,
            "fs.write",
            serde_json::json!({"path": path, "content": content}),
        )
        .await
    }
}

// ── host_fetch：经主控端出网（grants.http_fetch 域名白名单） ──────────────

pub struct HostFetchTool(pub HostBridge);

#[async_trait::async_trait]
impl Tool for HostFetchTool {
    fn name(&self) -> &str {
        "host_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL THROUGH the controller host's network (grants.http_fetch domain whitelist). \
         Use when this sandbox has no direct egress or the target domain is only reachable from \
         the controller. Returns {\"status\": N, \"body\": \"...\"}."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "url":{"type":"string","description":"Full URL (http/https)"},
            "max_bytes":{"type":"number","description":"Response body cap, default 1MB, max 10MB"}
        },"required":["url"]})
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> AgentResult<String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("missing url".into()))?;
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576);
        host_call(
            &self.0,
            "http.fetch",
            serde_json::json!({"url": url, "max_bytes": max_bytes}),
        )
        .await
    }
}
