# Worker 崩溃恢复设计文档

> **状态：设计稿** — 精准崩溃诊断 + 引导恢复。不自动重启，不复杂。崩溃后捕获 exit code + stderr，通知父 Worker，保留 Dead record，引导用户/coordinator 用 `--resume` 续接。

---

## 0. 核心哲学

**不做自动重启，只做精准诊断 + 可手动恢复。**

崩溃后的处理链：
```
Worker 进程退出
    ↓
读 exit code（之前从不读）
捕获 stderr（之前被 null 丢弃）
    ↓
exit_code == 0 → 正常退出 → 清理（同现状）
exit_code ≠ 0 → 崩溃/异常 → 标 Dead → 保留 record
    ↓
通知父 Worker：子任务崩溃，原因一二三
    ↓
父 Worker（或用户）决定是否重试
    ↓
续接：spawn_worker(session_id=旧id) 或 ion --resume <sid>
session 数据完整（only-append 不丢）
```

### 0.1 崩溃 vs 正常退出的判断

| exit_code | 含义 | 处理 |
|-----------|------|------|
| 0 | 正常退出（agent 自然结束、被 kill_worker 正常关闭） | 同现状：清理 record |
| 非 0 | 异常退出（panic、OOM、被 sigkill） | **新行为**：标 Dead、记录原因、通知父 |
| None（没产生） | spawn 失败/进程从未正常启动 | 同现状：清理 |

### 0.2 恢复方式

不需要"隐形自动重启"。崩溃后：
- **编排场景**：coordinator 收到 `child_crashed` 事件，可以重新 `spawn_worker(session_id=旧id)` 在同 session 继续
- **长会话**：用户看到提示 `Worker crashed. Session intact: ion --resume <sid>`，手动恢复

---

## 1. 配置

无新增配置项。全部行为在代码中固定（YAGNI）。

---

## 2. 主流程

### 2.1 stderr 捕获

**文件**：[src/worker_registry.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs) L251

```rust
// 修复前：
.stderr(Stdio::null())

// 修复后：
.stderr(Stdio::piped())
```

spawn 后，取 stderr 的读端写入日志文件：

```rust
let stderr = child.stderr.take().ok_or("no stderr")?;
let stderr_path = /* ~/.ion/tmp/ion-worker-{wid}.stderr */;
let stderr_path_clone = stderr_path.clone();
let stderr_wid = wid.clone();
tokio::spawn(async move {
    use tokio::io::AsyncBufReadExt;
    use std::io::Write;
    let mut reader = tokio::io::BufReader::new(stderr);
    let mut line = String::new();
    let mut lines_buf: VecDeque<String> = VecDeque::with_capacity(20);
    if let Some(parent) = std::path::Path::new(&stderr_path_clone).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        // 写日志
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(&stderr_path_clone)
        {
            let _ = f.write_all(line.as_bytes());
        }
        // 缓存最近 20 行（崩溃时取最后几行作为 exit_reason）
        lines_buf.push_back(line.clone());
        if lines_buf.len() > 20 { lines_buf.pop_front(); }
        line.clear();
    }
    // 保存最后几行 stderr 到 WorkerRecord（供崩溃时展示）
    let stderr_snippet: String = lines_buf.into_iter().collect();
    // 通过 channel 送回 stderr_snippet
});
```

### 2.2 exit code 读取

**文件**：[src/worker_registry.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs)（spawn 后，reader task 前）

spawn 后起一个 task 等 exit code：

```rust
let exit_code_task = tokio::spawn(async move {
    child.wait().await.ok().map(|s| s.code())
});
```

### 2.3 崩溃识别

stdout 关闭时（已有的 EOF 检测 L448），不直接 remove，而是：

```rust
// 修复前（L451）：
if let Some(mut record) = reg.workers.remove(&sub_wid) { ... }

// 修复后：
if let Some(mut record) = reg.workers.get_mut(&sub_wid) {
    // 读 exit code（如果 exit_code task 已完成）
    if let Ok(Some(Some(code))) = exit_code_rx.try_recv() {
        record.exit_code = Some(code);
        if code != 0 {
            record.status = WorkerStatus::Dead;
            // exit_reason
        }
    }
    if record.status != WorkerStatus::Dead {
        // 正常退出 → 清理
        reg.workers.remove(&sub_wid);
    }
    // Dead → 保留 record，后续清理靠定时 GC
}
```

### 2.4 父 Worker 通知

通过 `parent_event_tx`（已有的 channel，当前未使用）：

```rust
if let Some(ref parent_tx) = record.parent_event_tx {
    let event = serde_json::json!({
        "type": "child_crashed",
        "worker_id": child_wid,
        "session_id": record.session_id,
        "exit_code": record.exit_code,
        "exit_reason": record.exit_reason,
    });
    let _ = parent_tx.try_send(event);
}
```

`drain_until_agent_end` 收到此事件后提前返回错误（不再干等 300s）：

```rust
// 在 drain_until_agent_end 的事件循环里（L976 附近）
while let Some(event) = rx.recv().await {
    if event["type"] == "child_crashed" {
        return Err(format!(
            "Worker '{}' crashed (exit {})", 
            cwid, 
            event.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1)
        ));
    }
    // ... 原有事件处理 ...
}
```

### 2.5 Dead record 清理

不立即清理 Dead Worker。通过一个定期 GC task 自动清理超过 N 分钟的 Dead record：

```rust
/// 定期清理超时的 Dead/Stale record
pub async fn gc_dead_workers(&mut self, max_age_secs: u64) {
    let now = now_ms();
    let deadline = now - (max_age_secs * 1000) as i64;
    self.workers.retain(|_id, record| {
        if matches!(record.status, WorkerStatus::Dead | WorkerStatus::Stale) {
            if record.started_at < deadline {
                return false; // 移除超时的 Dead
            }
        }
        true
    });
}
```

在 `all_workers_idle` 检查（场景 2 退出条件）中，Dead Worker 不计入"idle"判定。

### 2.6 关键决策点

| 场景 | 处理 |
|------|------|
| Worker panic | exit_code ≠ 0 → Dead → stderr 有 panic 内容 → 通知父 |
| Worker OOM | 被 OS 杀掉 → exit_code ≠ 0（可能无 stderr）→ Dead |
| Worker 被 kill_worker | exit_code 通常是 0（正常关闭）→ 不标 Dead |
| Worker 正常结束 get_state | exit_code == 0 → 正常清理 |
| Session 不存在时重启 | spawn 失败（不是 crash）→ 不创建 Dead record |
| Dead record 过多 | GC task 自动清理（5 分钟超时） |

---

## 3. 数据结构变更

### 3.1 `WorkerRecord` 新增字段

**文件**：[src/worker_registry.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs#L30)

```rust
pub struct WorkerRecord {
    // ... 现有字段 ...
    
    /// 进程退出码（0 = 正常, 非 0 = 异常, None = 尚未退出）
    pub exit_code: Option<i32>,
    
    /// 退出原因（stderr 最后几行 + 退出码描述）
    pub exit_reason: Option<String>,
    
    /// stderr 日志文件路径（用于事后排查）
    pub stderr_path: Option<String>,
}
```

### 3.2 `WorkerStatus` 使用激活

`WorkerStatus::Dead` 和 `WorkerStatus::Stale` 当前是死代码。激活：

| 状态 | 设置时机 | 含义 |
|------|---------|------|
| `Idle` | spawn、agent_end（同现状） | Worker 正常工作，无事可做 |
| `Busy` | send_command（同现状） | Worker 正在处理请求 |
| `Dead` | **新**：崩溃检测到非 0 exit_code | Worker 进程异常退出 |
| `Stale` | **新**：定时 GC 前的过渡（可选） | 即将被清理的 Dead |
| `Paused` | 暂不启用（需要暂停功能时） | — |

---

## 4. 接口规格

### 4.1 Worker 列表展示 Dead Worker

**请求：**

```bash
ion workers  # 或 ion rpc --method get_state
```

**输出（新增 Dead 行）：**

```
WORKER ID         SESSION   STATUS   MODEL           EXIT CODE  EXIT REASON
a1b2c3d4          sess_xxx  Dead     -               -1         panic: unwrap failed at tool.rs:42
e5f6a7b8          sess_yyy  Busy     glm-4.7         -          -
```

### 4.2 崩溃通知（内部事件）

父 Worker 订阅到的事件：

```json
{
  "type": "child_crashed",
  "worker_id": "a1b2c3d4",
  "session_id": "sess_xxx",
  "exit_code": -1,
  "exit_reason": "panic: unwrap failed at tool.rs:42\n  at worker.rs:108",
  "message_count": 23
}
```

### 4.3 恢复提示（用户视角）

```bash
$ ion --host --agent coordinator "修复 bug"
...
⚠️ Worker 'a1b2c3d4' crashed (exit code: 134, signal: SIGABRT)
📋 Last stderr output:
   thread 'main' panicked at src/agent/tool.rs:42:
   called 'Option::unwrap()' on a 'None' value
📝 Session 'sess_xxx' is intact (23 messages preserved)
🔄 Resume with: ion --resume sess_xxx --fork-from-leaf <last> "继续"
```

---

## 5. CLI 测试指南

### Group A：崩溃检测 + 诊断

#### A1 stderr 捕获

**前置：** 准备一个会让 ion-worker panic 的请求。

```bash
# 启动一个 Worker，让它执行会 panic 的工具
# （用 faux 模拟异常场景不直接——需要真实 panic）
# 验证：stderr 文件存在且有内容
ls -la ~/.ion/tmp/ion-worker-*.stderr
```

**预期：** stderr 文件存在，包含 panic 信息。

#### A2 exit code 读取

```bash
# Worker 异常退出后
ion rpc --method get_state
```

**预期：** 该 Worker 的 status 为 "Dead"，exit_code 非 0。

### Group B：父 Worker 通知

#### B1 coordinator 收到崩溃通知

**前置：** coordinator spawn 了子 Worker，子 Worker 崩溃。

```bash
# 通过事件订阅观察
ion subscribe --session <coordinator-sid>
```

**预期：** 收到 `child_crashed` 事件，含 exit_code 和 exit_reason。

### Group C：恢复

#### C1 续接崩溃 Worker 的 session

```bash
# 崩溃后
ion index sessions | grep <crashed-worker-session>
ion --resume <sid> "继续运行"
```

**预期：** 新 Worker 加载旧 session，继承 parentId 链。

### Group D：单元测试

| 测试 | 验证点 |
|------|--------|
| `test_exit_code_zero_cleans_up` | exit_code=0 正常清理 |
| `test_exit_code_nonzero_marks_dead` | exit_code≠0 标 Dead |
| `test_parent_notified_on_crash` | 父收到 child_crashed |
| `test_drain_returns_error_on_child_crash` | drain_until_agent_end 崩溃后立即返回错误 |
| `test_gc_removes_old_dead` | Dead 超过时限被清理 |

---

## 6. 实现顺序

| Phase | 内容 | 预估 |
|-------|------|------|
| 1 | stderr 捕获（Stdio::piped + 写文件 + 缓存最后 N 行） | 0.5 天 |
| 2 | exit code task + 崩溃识别（非 0 → Dead） | 0.5 天 |
| 3 | WorkerRecord 加字段 + 保留 Dead record | 0.5 天 |
| 4 | 父 Worker 通知（child_crashed 事件 + drain_until_agent_end 改进） | 1 天 |
| 5 | Dead 展示（worker list + exit_code/reason） + 引导恢复 | 0.5 天 |
| 6 | GC 定时清理超时 Dead record | 0.5 天 |
| 7 | 单元测试 + CI | 0.5 天 |
| **合计** | | **~4 天** |

---

## 7. 不做（明确排除）

| 功能 | 排除理由 |
|------|---------|
| 自动重启/管家（supervisor/restart_policy） | 你说"不用搞太复杂"——诊断够了 |
| 心跳检测（lasth eartbeat watchdog） | stdout EOF + exit_code 已能检测崩溃 |
| 重启 in-flight 工具调用 | 少数场景，复杂性高 |
| `ION_RESTART_*` 环境变量传递 | 无自动重启，不需要 |
| WorkerCreateConfig.restart_policy 字段 | 无自动重启，不需要 |
