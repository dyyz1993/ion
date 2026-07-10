//! File Snapshot — 双路文件快照系统
//!
//! 路线 1：write/edit 工具级 before/after（100% 精确 diff）
//! 路线 2：bash 目录扫描兜底（mtime+size 启发式）
//!
//! 存储约束：content hash 去重 + 100MB 上限 + 分级 GC
//! 性能约束：mtime+size 快速过滤 + 按 turnId 索引

pub mod object_store;
pub mod snapshot;
pub mod scanner;
pub mod diff;
pub mod gc;

pub use object_store::{ObjectStore, project_key, content_hash};
pub use snapshot::{
    ToolSnapshot, DirSnapshot, DirFileChange, ChangeStatus,
    BeforeState, SnapshotStore, capture_before, capture_after, capture_after_dir,
};
pub use scanner::{scan_dir_fast, is_binary, DirScanResult};
pub use diff::{unified_diff, count_diff};

use crate::agent::extension::Extension;
use crate::agent::error::AgentResult;
use std::collections::HashMap;
use std::sync::Mutex;

/// File Snapshot 扩展 — 通过 on_tool_execution_start/end 钩子采集
pub struct FileSnapshotExtension {
    /// session 的 cwd
    #[allow(dead_code)]
    cwd: String,
    /// project key
    #[allow(dead_code)]
    project_key: String,
    /// 快照存储（Arc 共享，RPC 层也能访问）
    store: std::sync::Arc<SnapshotStore>,
    /// 当前 turn 的 before 状态（tool_call_id → BeforeState）
    before_states: Mutex<HashMap<String, BeforeState>>,
    /// 当前 turn 的唯一 ID（如 "ts_a3f8b2"，on_turn_start 时生成）
    current_turn_id: Mutex<String>,
}

/// 生成 6 位随机 hex turn ID（如 "ts_a3f8b2"）
fn gen_turn_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("ts_{:06x}", hasher.finish() & 0xFFFFFF)
}

impl FileSnapshotExtension {
    /// 创建并返回 (extension, store_arc)
    pub fn new_pair(cwd: &str) -> (Self, std::sync::Arc<SnapshotStore>) {
        let pk = project_key(cwd);
        let store = std::sync::Arc::new(SnapshotStore::new(&pk));
        let ext = Self {
            cwd: cwd.to_string(),
            project_key: pk,
            store: store.clone(),
            before_states: Mutex::new(HashMap::new()),
            current_turn_id: Mutex::new(String::new()),
        };
        (ext, store)
    }

    /// 获取快照存储（RPC 查询用）
    pub fn store(&self) -> &SnapshotStore {
        &self.store
    }
}

#[async_trait::async_trait]
impl Extension for FileSnapshotExtension {
    fn name(&self) -> &str { "file_snapshot" }

    async fn on_turn_start(
        &self,
        _ctx: &mut crate::agent::extension::TurnContext,
    ) -> AgentResult<()> {
        // 每轮 turn 生成唯一 ID（不依赖下标递增）
        *self.current_turn_id.lock().unwrap() = gen_turn_id();
        Ok(())
    }

    async fn on_tool_execution_start(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        let before = capture_before(&ctx.tool_name, &ctx.args, self.store.objects(), &self.cwd);
        if !matches!(before, BeforeState::Skip) {
            self.before_states.lock().unwrap().insert(ctx.tool_call_id.clone(), before);
        }
        Ok(())
    }

    async fn on_tool_execution_end(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> AgentResult<()> {
        let turn_id = self.current_turn_id.lock().unwrap().clone();
        let before = self.before_states.lock().unwrap().remove(&ctx.tool_call_id);

        if let Some(before_state) = before {
            match &before_state {
                BeforeState::FileCapture { .. } => {
                    if let Some(snap) = capture_after(
                        &before_state, self.store.objects(),
                        &turn_id, &ctx.tool_call_id, &ctx.tool_name,
                    ) {
                        self.store.save_tool_snapshot(&snap);
                    }
                }
                BeforeState::DirCapture { .. } => {
                    let snaps = capture_after_dir(
                        &before_state, self.store.objects(), &self.cwd,
                        &turn_id, &ctx.tool_call_id,
                    );
                    for snap in snaps {
                        self.store.save_tool_snapshot(&snap);
                    }
                }
                BeforeState::Skip => {}
            }
        }
        Ok(())
    }
}
