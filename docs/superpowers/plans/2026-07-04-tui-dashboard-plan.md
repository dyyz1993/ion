# ION TUI Dashboard 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建 `ion dashboard` TUI 仪表板，可以在一个面板中实时监控多 Worker 运行状态、查看输出、聊天交互，带赛博朋克主题和响应式布局。

**Architecture:**
- 内核补丁（`WorkerRecord` 状态流转 + `subscribe_overview` 事件流）为 TUI 提供准确数据
- `src/tui/` 模块用 ratatui + crossterm 实现 TUI，通过 Unix socket 连 Manager
- 三路事件循环：Manager 事件 | 终端事件 | tick 时钟 → `AppState` → 渲染
- 响应式分栏：Wide(≥140)三栏 / Medium(80-139)两栏 / Narrow(<80)单栏

**Tech Stack:** Rust, ratatui 0.29, crossterm 0.28, tokio, serde_json

---

### 文件结构

**新文件（12 个，全部 src/tui/ 下）:**

| 文件 | 职责 |
|------|------|
| `src/tui/mod.rs` | `run_dashboard()` 入口 + 模块声明 |
| `src/tui/app.rs` | `AppState` + 事件循环（select! Manager/terminal/tick） |
| `src/tui/manager_conn.rs` | Unix socket 客户端：subscribe_overview / subscribe_session / send |
| `src/tui/theme.rs` | 赛博朋克调色板 + Style 常量 |
| `src/tui/layout.rs` | 响应式分栏计算 |
| `src/tui/state.rs` | 草稿/折叠状态持久化 |
| `src/tui/view/mod.rs` | 渲染分发 |
| `src/tui/view/tree.rs` | 项目树 widget |
| `src/tui/view/kanban.rs` | 卡片网格 widget |
| `src/tui/view/detail.rs` | 详情/Focus 模式（聊天 + 侧栏） |
| `src/tui/view/chat_input.rs` | 输入框 + 草稿管理 |
| `src/tui/view/todo_panel.rs` | Todo 面板（占位版） |

**修改文件（3 个）:**

| 文件 | 改动 |
|------|------|
| `src/worker_registry.rs` | 状态流转 + overview 广播 + latest_output 字段 |
| `src/bin/ion.rs` | + subscribe_overview 命令 + dashboard 子命令 |
| `src/lib.rs` | + `pub mod tui` |
| `Cargo.toml` | + ratatui, crossterm |

---

### Task 0.1: WorkerRecord 加 latest_output 字段 + 状态流转

**Files:**
- Modify: `src/worker_registry.rs:26-51` (WorkerRecord struct)
- Modify: `src/worker_registry.rs:74-81` (WorkerStatus — 已有 Stale，不用改 enum)
- Modify: `src/worker_registry.rs:480-488` (reclaim — 不变)
- Modify: `src/worker_registry.rs:369-383` (emit_global → 改为 broadcast_overview)
- Modify: `src/worker_registry.rs:580-602` (send_command 加 status → Busy)
- Modify: `src/worker_registry.rs:1201-1285` (read_worker_stdout 加 agent_end → Idle)

- [ ] **Step 1: 往 WorkerRecord 加 latest_output 和 model_size 字段**

```rust
// src/worker_registry.rs, WorkerRecord struct (after line 50, before worktree)
pub latest_output: VecDeque<String>,       // MAX 5 lines
pub log_short: Option<String>,              // 一行摘要（最新 text_delta 截断）
pub model_size: Option<String>,             // e.g. "128k"
```

在 `create_worker` 里初始化（`new` 路径）:
```rust
// 在 create_worker 返回前设置默认值
record.latest_output = VecDeque::with_capacity(5);
record.log_short = None;
```

- [ ] **Step 2: send_command 设 Busy + broadcast_overview**

在 `send_command` 里写入 stdin 之后、`Ok(())` 之前:
```rust
// 状态切换：收到命令 → Busy
record.status = WorkerStatus::Busy;
drop(record); // 放锁再广播（用 extra scope 也行）
// broadcast_overview 需要 &self 但我们在 &mut self send_command 里
// 解决方案：跳过广播，让调用方（handle_manager_command）负责 broadcast
// 在 handle_manager_command 的 "send" 分支做完后广播
```
改为在 `handle_manager_command`（src/bin/ion.rs）的 `"send"`/`"send_to_session"` 分支后广播。
因为 send_command 是 &mut self 内部方法，广播需要等锁释放。

```rust
// handle_manager_command, after reg.send_to_session(...) or reg.send_command(...) success:
drop(reg); // 放锁
registry.lock().await.broadcast_overview(); // 重获锁广播
```

- [ ] **Step 3: read_worker_stdout 检测 agent_end → Idle**

在 `src/worker_registry.rs` `read_worker_stdout` 函数的 JSON 解析循环里（约 line 1240-1277），找到 event type 检测。在已知的 msg_type 分支后加:

```rust
// 在 msg_type 匹配末尾（line ~1277），else if 之后
if msg_type == "event" {
    if let Some(ev) = msg.get("event") {
        if let Some(et) = ev.get("type").and_then(|v| v.as_str()) {
            if et == "agent_end" {
                if let Some(record) = reg.workers.get_mut(&worker_id) {
                    record.status = WorkerStatus::Idle;
                }
                // 广播（需要放锁或另起方式）
                let reg_clone = registry.clone();
                let wid = worker_id.clone();
                tokio::spawn(async move {
                    let mut r = reg_clone.lock().await;
                    r.broadcast_overview();
                });
            }
        }
    }
}
```

同时在这个分支里收集 text_delta 到 latest_output:
```rust
if et == "text_delta" {
    if let Some(delta) = ev.get("delta").and_then(|v| v.as_str()) {
        if let Some(record) = reg.workers.get_mut(&worker_id) {
            record.latest_output.push_back(delta.to_string());
            while record.latest_output.len() > 5 {
                record.latest_output.pop_front();
            }
            record.log_short = Some(
                delta.chars().take(60).collect::<String>()
            );
        }
    }
}
```

- [ ] **Step 4: get_overview 输出 latest_output 和 log_short**

```rust
// src/worker_registry.rs, get_overview() line ~1156, workers.map 闭包里:
let latest_output: Vec<&str> = w.latest_output.iter().map(|s| s.as_str()).collect();
serde_json::json!({
    "worker_id": w.worker_id,
    "session_id": w.session_id,
    "project": w.project,
    "status": w.status,
    "model": w.model,
    "model_size": w.model_size,
    "agent": w.agent,
    "channels": w.channels,
    "parent": w.parent,
    "children": w.children,
    "latest_output": latest_output,
    "log_short": w.log_short,
    "started_at": w.started_at,
})
```

- [ ] **Step 5: 编译验证**

```bash
cargo build --bin ion
```

Expected: 编译通过，`get_overview` response 里多了 `latest_output`、`log_short`、`model_size` 字段。

- [ ] **Step 6: Commit**

```bash
git add src/worker_registry.rs
git commit -m "feat(registry): add latest_output, log_short, model_size to WorkerRecord, add agent_end→Idle transition"
```

---

### Task 0.2: subscribe_overview 内核事件流

**Files:**
- Modify: `src/worker_registry.rs` (add overview_subscribers + broadcast_overview)
- Modify: `src/bin/ion.rs` (socket handler add "subscribe_overview" command)

- [ ] **Step 1: WorkerRegistry 加 overview_subscribers**

```rust
// WorkerRegistry struct (after global_subscribers line 20)
pub overview_subscribers: Vec<mpsc::UnboundedSender<serde_json::Value>>,
```

初始化（`new()`):
```rust
overview_subscribers: Vec::new(),
```

- [ ] **Step 2: broadcast_overview() 方法**

```rust
// 在 WorkerRegistry impl 内, 放在 subscribe_global 后面（~line 1152）
pub fn broadcast_overview(&self) {
    let overview = self.get_overview();
    self.overview_subscribers.retain(|tx| tx.send(overview.clone()).is_ok());
}
```

- [ ] **Step 3: subscribe_overview() 方法**

```rust
// 在 broadcast_overview 上面
pub fn subscribe_overview(&mut self) -> mpsc::UnboundedReceiver<serde_json::Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    self.overview_subscribers.push(tx);
    // 立即推送当前状态
    let overview = self.get_overview();
    let _ = rx.try_send(todo!("we need to send initial snapshot differently"));
    // 实际做法：先 push tx，drop lock，然后外部先 get_overview 再等流
    rx
}
```

Better approach — 订阅者先读初始状态，后续等流:
```rust
pub fn subscribe_overview(&mut self) -> mpsc::UnboundedReceiver<serde_json::Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    self.overview_subscribers.push(tx);
    // 初始快照通过调用者先 get_overview 获取
    // 这里只设好 channel
    rx
}
```

然后在此前调用点（socket handler）先返回 overview，再持续推送:
```rust
// socket handler 会:
// 1. lock registry
// 2. get_overview → 返回 initial data
// 3. subscribe_overview → 拿 rx
// 4. drop lock
// 5. 回复 initial data
// 6. loop rx.recv() 推后续变更
```

- [ ] **Step 4: 在 create_worker / kill_worker 末尾调 broadcast_overview**

```rust
// create_worker (在 emit_global 后面, line ~385):
self.broadcast_overview();

// kill_worker (在 emit_global 后面):
self.broadcast_overview();
```

- [ ] **Step 5: socket handler 加 subscribe_overview 命令**

在 `src/bin/ion.rs` socket accept loop 里，`handle_manager_command` 和 `subscribe` 之间的代码。在 `if method == "subscribe"` 的分支后面（line ~1646）加:

```rust
// ── Overview stream: subscribe_overview ──
if method == "subscribe_overview" {
    let initial = {
        let reg = reg.lock().await;
        let overview = reg.get_overview();
        let rx = reg.subscribe_overview();
        (overview, rx)
    };
    // 返回初始快照
    let ack = serde_json::json!({
        "type": "response", "id": cmd.get("id"),
        "success": true,
        "data": {
            "stream": "overview",
            "initial": initial.0,
        }
    });
    let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
    let _ = write_half.flush().await;
    // 持续推后续变更
    let mut rx = initial.1;
    loop {
        match rx.recv().await {
            Some(snapshot) => {
                let msg = serde_json::json!({
                    "type": "overview_snapshot",
                    "data": snapshot,
                });
                if write_half.write_all(format!("{msg}\n").as_bytes()).await.is_err() {
                    break;
                }
                let _ = write_half.flush().await;
            }
            None => break,
        }
    }
    return;
}
```

注意：这里 `reg` 是 `Arc<Mutex<WorkerRegistry>>`，`subscribe_overview` 返回 rx 后必须 drop lock 才读 rx。

- [ ] **Step 6: 编译验证**

```bash
cargo build --bin ion
```

然后 `ion serve start` 再 `echo '{"method":"subscribe_overview","id":"test1"}' | nc -U ~/.ion/host.sock` 测试。

- [ ] **Step 7: Commit**

```bash
git add src/worker_registry.rs src/bin/ion.rs
git commit -m "feat(manager): add subscribe_overview streaming endpoint"
```

---

### Task 0.3: Manager 端 heartbeat 检测 + Stale 状态

**Files:**
- Modify: `src/bin/ion.rs:1820-1828` (Manager 后台任务)

- [ ] **Step 1: 加 heartbeat 检查后台任务**

在 `cmd_manager_start` 的后台任务 3（转发全局事件）后面加:

```rust
// 后台任务 4：heartbeat 检查，标记 Stale
let hb_registry = Arc::clone(&registry);
tokio::spawn(async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        let mut reg = hb_registry.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let mut changed = false;
        for record in reg.workers.values_mut() {
            if record.status != WorkerStatus::Dead && record.status != WorkerStatus::Stale {
                if now - record.last_heartbeat > 60_000 {
                    record.status = WorkerStatus::Stale;
                    tracing::warn!("[{}] heartbeat timeout → Stale", record.worker_id);
                    changed = true;
                }
            }
        }
        if changed {
            reg.broadcast_overview();
        }
    }
});
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build --bin ion
git add src/bin/ion.rs
git commit -m "feat(manager): add heartbeat stale detection"
```

---

### Task 1.1: 新增 ratatui/crossterm 依赖 + lib.rs 模块声明

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`

- [ ] **Step 1: Cargo.toml 加依赖**

```toml
# 在 axum 后面加
ratatui = "0.29"
crossterm = "0.28"
```

- [ ] **Step 2: lib.rs 加模块**

```rust
// 按字母序在 session_index 后面加
pub mod tui;
```

- [ ] **Step 3: 编译验证**

```bash
cargo build --bin ion
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs
git commit -m "feat(tui): add ratatui/crossterm deps and module declaration"
```

---

### Task 1.2: src/tui/mod.rs — run_dashboard 入口

**Files:**
- Create: `src/tui/mod.rs`

- [ ] **Step 1: 写模块入口**

```rust
// src/tui/mod.rs
pub mod app;
pub mod layout;
pub mod manager_conn;
pub mod theme;
pub mod state;
pub mod view;

use anyhow::Result;

/// 启动 Dashboard TUI
pub async fn run_dashboard() -> Result<()> {
    // 1. 连 Manager socket
    let mut conn = manager_conn::ManagerConn::connect().await
        .map_err(|e| anyhow::anyhow!("Cannot connect to Manager: {e}"))?;
    
    // 2. 获取初始 overview
    let initial = conn.request_overview().await?;
    
    // 3. 订阅 overview 事件流
    let overview_rx = conn.subscribe_overview().await?;
    
    // 4. 初始化 AppState
    let mut app = app::AppState::new(initial, overview_rx);
    
    // 5. 进入 TUI 事件循环
    app.run().await
}
```

注意：这里用 `anyhow::Result` — 如果不想加 anyhow 依赖，改回 `Result<(), Box<dyn std::error::Error>>`。

- [ ] **Step 2: 编译验证（无实际功能，只验证模块加载）**

```bash
cargo build --bin ion
```

- [ ] **Step 3: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat(tui): add dashboard entry point"
```

---

### Task 1.3: theme.rs — 赛博朋克调色板

**Files:**
- Create: `src/tui/theme.rs`

- [ ] **Step 1: 定义 Theme 结构体和调色板常量**

```rust
// src/tui/theme.rs
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Color,
    pub panel_bg: Color,
    pub text: Color,
    pub subtext: Color,
    pub accent: Color,
    pub danger: Color,
    pub warning: Color,
    pub dead: Color,
    pub border_focused: Color,
    pub border_normal: Color,
    pub border_inactive: Color,
}

impl Theme {
    pub const fn cyberpunk() -> Self {
        Self {
            bg: Color::Rgb(0x0a, 0x0e, 0x1a),
            panel_bg: Color::Rgb(0x11, 0x18, 0x27),
            text: Color::Rgb(0xc8, 0xd3, 0xf5),
            subtext: Color::Rgb(0x5b, 0x6b, 0x9c),
            accent: Color::Rgb(0x00, 0xff, 0xd1),
            danger: Color::Rgb(0xff, 0x2d, 0x95),
            warning: Color::Rgb(0xff, 0xb8, 0x00),
            dead: Color::Rgb(0x7a, 0x1f, 0x3d),
            border_focused: Color::Rgb(0x00, 0xff, 0xd1),
            border_normal: Color::Rgb(0x1f, 0x4d, 0x5c),
            border_inactive: Color::Rgb(0x2a, 0x33, 0x49),
        }
    }
}

/// 快捷函数：创建带主题背景的面板 block
pub fn panel_block<'a>(title: &'a str, theme: &Theme, focused: bool) -> ratatui::widgets::Block<'a> {
    let border_color = if focused {
        theme.border_focused
    } else {
        theme.border_inactive
    };
    ratatui::widgets::Block::default()
        .title(ratatui::widgets::block::Title::from(title))
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel_bg))
}

/// 状态对应的显示字符和颜色
pub fn status_icon(status: &str) -> (&'static str, Style) {
    let theme = Theme::cyberpunk();
    match status {
        "busy" => ("▶", Style::default().fg(theme.danger).add_modifier(Modifier::BOLD)),
        "idle" => ("⏸", Style::default().fg(theme.accent)),
        "stale" => ("⚠", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        "dead" => ("⨯", Style::default().fg(theme.dead)),
        _ => ("?", Style::default().fg(theme.subtext)),
    }
}
```

- [ ] **Step 2: 编译验证**

```bash
cargo build --bin ion
```

- [ ] **Step 3: Commit**

```bash
git add src/tui/theme.rs
git commit -m "feat(tui): add cyberpunk theme palette"
```

---

### Task 1.4: layout.rs — 响应式分栏

**Files:**
- Create: `src/tui/layout.rs`

- [ ] **Step 1: 定义布局类型 + 计算函数**

```rust
// src/tui/layout.rs
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// 布局档次
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutTier {
    Wide,   // ≥140 列 — 三栏
    Medium, // 80-139 列 — 两栏
    Narrow, // <80 列 — 单栏
}

/// 各面板的 Rect 分配
#[derive(Debug)]
pub struct AppLayout {
    pub tier: LayoutTier,
    pub tree_rect: Option<Rect>,
    pub kanban_rect: Option<Rect>,
    pub detail_rect: Option<Rect>,
    pub input_bar_rect: Option<Rect>,
    pub status_bar_rect: Rect,
}

impl LayoutTier {
    pub fn from_width(width: u16) -> Self {
        if width >= 140 {
            Self::Wide
        } else if width >= 80 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }
}

/// 计算布局（不依赖 AppState，纯尺寸驱动）
pub fn compute_layout(area: Rect, tier: LayoutTier, focus_mode: bool) -> AppLayout {
    let status_bar_height = 1;
    let input_bar_height = 3;

    let (main_area, status_bar_rect) = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(status_bar_height),
        ])
        .split(area)
        .into_iter()
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
        .fold((area, Rect::default()), |(main, status), (i, r)| {
            if i == 0 { (r, status) } else { (main, r) }
        });
    // Simpler:
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(area);
    let main = chunks[0];
    let status_bar_rect = chunks[1];

    if focus_mode {
        // Focus 模式：detail 占 ≥70%，右侧栏合并到 detail 内部
        let (mut detail_rect, _) = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1)])
            .split(main)
            .into_iter()
            .collect::<Vec<_>>()
            .into_iter()
            .enumerate()
            .fold((Rect::default(), Rect::default()), |(d, _), (i, r)| {
                if i == 0 { (r, d) } else { (d, r) }
            });
        // Hmm, simpler:
        return AppLayout {
            tier,
            tree_rect: None,
            kanban_rect: None,
            detail_rect: Some(main),
            input_bar_rect: None,
            status_bar_rect,
        };
    }

    match tier {
        LayoutTier::Wide => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(28),
                    Constraint::Fill(1),
                    Constraint::Length(40),
                ])
                .split(main);
            AppLayout {
                tier,
                tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: Some(cols[2]),
                input_bar_rect: None,
                status_bar_rect,
            }
        }
        LayoutTier::Medium => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(24),
                    Constraint::Fill(1),
                ])
                .split(main);
            AppLayout {
                tier,
                tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: None,
                input_bar_rect: None,
                status_bar_rect,
            }
        }
        LayoutTier::Narrow => {
            AppLayout {
                tier,
                tree_rect: None,
                kanban_rect: Some(main),
                detail_rect: None,
                input_bar_rect: None,
                status_bar_rect,
            }
        }
    }
}

/// 计算一行能放几张卡片（基于卡片最小宽 40 列）
pub fn cards_per_row(width: u16) -> usize {
    let available = width.saturating_sub(4); // padding
    std::cmp::max(1, available as usize / 42)
}
```

- [ ] **Step 2: 编译**

```bash
cargo build --bin ion
```

- [ ] **Step 3: Commit**

```bash
git add src/tui/layout.rs
git commit -m "feat(tui): add responsive layout computation"
```

---

### Task 1.5: manager_conn.rs — Unix socket 客户端

**Files:**
- Create: `src/tui/manager_conn.rs`

- [ ] **Step 1: 定义 ManagerConn 和事件类型**

```rust
// src/tui/manager_conn.rs
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Manager 推来的事件
#[derive(Debug)]
pub enum ManagerEvent {
    OverviewSnapshot(Value),
    InstanceEvent { session: String, event: Value },
    Disconnected,
}

/// Unix socket 客户端 — 连接 ION Manager
pub struct ManagerConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    line_buf: String,
}

impl ManagerConn {
    /// 连接 Manager Unix socket
    pub async fn connect() -> Result<Self, String> {
        let sock_path = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock_path).await
            .map_err(|e| format!("Cannot connect to {}: {e}", sock_path.display()))?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            line_buf: String::new(),
        })
    }

    /// 发送 JSON 请求，读一行 response
    pub async fn send_request(&mut self, req: &Value) -> Result<Value, String> {
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await
            .map_err(|e| format!("write: {e}"))?;
        self.writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        self.read_line().await
    }

    async fn read_line(&mut self) -> Result<Value, String> {
        self.line_buf.clear();
        self.reader.read_line(&mut self.line_buf).await
            .map_err(|e| format!("read: {e}"))?;
        if self.line_buf.is_empty() {
            return Err("connection closed".into());
        }
        serde_json::from_str(self.line_buf.trim())
            .map_err(|e| format!("parse: {e}"))
    }

    /// 取 overview 快照（get_overview）
    pub async fn request_overview(&mut self) -> Result<Value, String> {
        let req = serde_json::json!({"method": "get_overview", "id": "tui-init"});
        let resp = self.send_request(&req).await?;
        // 格式: {"type":"response","success":true,"data":{...}}
        Ok(resp.get("data").cloned().unwrap_or(resp))
    }

    /// 订阅 overview 事件流。返回 receiver，每次推变化
    pub async fn subscribe_overview(&mut self) -> Result<mpsc::UnboundedReceiver<Value>, String> {
        let req = serde_json::json!({"method": "subscribe_overview", "id": "tui-ov"});
        // 写请求，读 response ack
        let ack = self.send_request(&req).await?;
        if !ack.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(ack.get("error").and_then(|v| v.as_str()).unwrap_or("subscribe_overview failed").into());
        }
        // 返回一个 mpsc channel，让事件循环去持续读 socket
        // 实际会把 self.reader 的读线程和 channel 绑定
        let (tx, rx) = mpsc::unbounded_channel();
        // 这里需要把 socket reader 传给一个后台 task 去持续读
        // 但我不能 move self，因为还需要其他方法（subscribe_session, send_prompt）
        // 方案：单独 spawn 一个 task 读单独 socket 连接
        Err("TODO: need refactor for streaming - use two connections".into())
    }
}
```

实际上这个设计有 ownership 问题 — `subscribe_overview` 需要持续读 socket，但 `ManagerConn` 还需要发其他请求。标准方案是**两个 socket 连接**：一个控制连接（请求/响应），一个事件连接（持续读）。或一个连接但用 tokio 的 `split` + 后台 task 转发事件。

重构设计：

```rust
// 更好：ManagerConn 持单个连接，split 为 reader/writer
// writer 同步发请求，reader 由后台 task 持续读
// subscribe 时 reader task 识别 event 并转发

pub struct ManagerConn {
    writer: tokio::net::unix::WriteHalf,
    // 后台 task 持续从 reader 读，发现 event/overview 就转发到对应 channel
    _reader_task: tokio::task::JoinHandle<()>,
    event_tx: mpsc::UnboundedSender<ManagerEvent>,
}
```

为了简化，实际实现可以走**两个连接**：
1. 控制连接（单次请求-响应，`get_overview`、`send_prompt`）
2. 事件连接（`subscribe_overview`，长期存活，输出到 channel）

```rust
/// 最终方案：两个连接
pub struct ManagerConn {
    ctrl: CtrlConn,      // 请求-响应（get_overview, send）
    events: EventConn,   // 持续读（subscribe_overview, subscribe_session）
}

struct CtrlConn {
    stream: UnixStream,
    line_buf: String,
}

struct EventConn {
    reader: BufReader<UnixStream>,
    rx: mpsc::UnboundedReceiver<ManagerEvent>,
}
```

- [ ] **Step 2: 正确定义 CtrlConn + EventConn**

```rust
// src/tui/manager_conn.rs
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum ManagerEvent {
    /// Overview 快照变更
    OverviewSnapshot(Value),
    /// Worker 事件流（subscribe_session）
    InstanceEvent { session: String, event: Value },
    /// 连接断开
    Disconnected,
}

/// 控制连接 — 短请求/响应
struct CtrlConn {
    stream: UnixStream,
    buf: String,
}

impl CtrlConn {
    async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await
            .map_err(|e| format!("connect: {e}"))?;
        Ok(Self { stream, buf: String::new() })
    }

    async fn request(&mut self, req: &Value) -> Result<Value, String> {
        let line = format!("{req}\n");
        self.stream.write_all(line.as_bytes()).await
            .and_then(|_| self.stream.flush())
            .map_err(|e| format!("write: {e}"))?;
        self.buf.clear();
        let mut reader = BufReader::new(&mut self.stream);
        reader.read_line(&mut self.buf).await
            .map_err(|e| format!("read: {e}"))?;
        if self.buf.is_empty() { return Err("closed".into())); }
        serde_json::from_str(self.buf.trim()).map_err(|e| format!("parse: {e}"))
    }
}

/// 事件连接 — 持续读，发到 channel
struct EventConn {
    reader: BufReader<UnixStream>,
}

impl EventConn {
    async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await
            .map_err(|e| format!("connect: {e}"))?;
        let reader = BufReader::new(stream);
        Ok(Self { reader })
    }

    async fn send_subscribe(&mut self, req: &Value) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        // 需要 write half — 或者从 reader 的 inner() 拿
        // BufReader 不暴露 write... 得用 split
        Err("TODO: use split stream".into())
    }
}
```

EventConn 需要 split stream，这比较标准:

```rust
struct EventConn {
    reader: tokio::io::BufReader<tokio::net::unix::ReadHalf<>>,
    writer: tokio::net::unix::WriteHalf,
}

impl EventConn {
    async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
        })
    }

    async fn send_subscribe(&mut self, req: &Value) -> Result<(), String> {
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        self.writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// 读一行
    async fn read_line(&mut self, buf: &mut String) -> Result<Option<Value>, String> {
        buf.clear();
        match self.reader.read_line(buf).await {
            Ok(0) => Ok(None),
            Ok(_) => serde_json::from_str(buf.trim()).map(Some).map_err(|e| format!("parse: {e}")),
            Err(e) => Err(format!("read: {e}")),
        }
    }
}
```

不过先把代码写对编译。让我写最终版：

- [ ] **Step 2: 完整 manager_conn.rs 实现**

```rust
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Manager 推来的事件
#[derive(Debug)]
pub enum ManagerEvent {
    OverviewSnapshot(Value),
    InstanceEvent { session: String, event: Value },
    Disconnected,
}

/// 管理两个 socket 连接 + 事件 task
pub struct ManagerConn {
    ctrl_tx: mpsc::UnboundedSender<Value>,
    ctrl_rx: mpsc::UnboundedReceiver<Value>,
    event_rx: mpsc::UnboundedReceiver<ManagerEvent>,
    // 保留 sender 方便外面 clone 后跨 task 用
    event_sender: mpsc::UnboundedSender<ManagerEvent>,
}
```

Actually this is getting complex. Let me simplify — for MVP, ManagerConn 只需要三类操作，可以用一个连接 + 轮询完成（不需要 subscribe_overview 也能工作）：

**MVP 简化方案（不用 subscribe_overview，用轮询）：**

ManagerConn 单连接 + 轮询 get_overview + subscribe session 走单独线程读：

```rust
pub struct ManagerConn {
    stream: UnixStream,
    buf: String,
}

impl ManagerConn {
    pub async fn connect() -> Result<Self, String> { ... }
    
    /// 轮询 get_overview（供 AppState tick 调用）
    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        let req = json!({"method":"get_overview","id":"poll"});
        self.request(&req).await
    }
    
    /// 发送聊天消息
    pub async fn send_prompt(&mut self, session: &str, text: &str) -> Result<(), String> {
        let req = json!({"method":"send","session":session,"rpc_method":"prompt","params":{"text":text}});
        self.request(&req).await?;
        Ok(())
    }
    
    async fn request(&mut self, req: &Value) -> Result<Value, String> {
        let line = format!("{req}\n");
        self.stream.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        self.stream.flush().await.map_err(|e| format!("flush: {e}"))?;
        self.buf.clear();
        let mut reader = BufReader::new(&mut self.stream);
        reader.read_line(&mut self.buf).await.map_err(|e| format!("read: {e}"))?;
        if self.buf.is_empty() { return Err("closed".into())); }
        serde_json::from_str(self.buf.trim()).map_err(|e| format!("parse: {e}"))
    }
}
```

Hmm but `BufReader::new(&mut self.stream)` won't work because `into_split` moves the stream. And `request` needs to both read and write.

Simple fix — don't use BufReader for the request/response path, use raw stream:

```rust
async fn request(&mut self, req: &Value) -> Result<Value, String> {
    let line = format!("{req}\n");
    self.stream.writable().await.unwrap();
    self.stream.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
    // Read response: read until newline
    let mut buf = vec![0u8; 4096];
    let n = self.stream.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
    if n == 0 { return Err("closed".into())); }
    let s = std::str::from_utf8(&buf[..n]).map_err(|e| format!("utf8: {e}"))?;
    serde_json::from_str(s.trim()).map_err(|e| format!("parse: {e}"))
}
```

The issue is that `read` on a UnixStream reads arbitrary bytes, and the response might come fragmented. For a single-line JSONL response, `read_line` is better. But `BufReader` requires ownership.

Simplest working approach: borrow the stream with Pin:

```rust
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

async fn request<'a>(&'a mut self, req: &Value) -> Result<Value, String> {
    let line = format!("{req}\n");
    let (mut r, mut w) = Pin::new(&mut self.stream).split();
    w.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
    w.flush().await.ok();
    drop(w);
    let mut buf = String::new();
    let mut reader = tokio::io::BufReader::new(&mut r);
    reader.read_line(&mut buf).await.map_err(|e| format!("read: {e}"))?;
    let _ = reader;
    if buf.is_empty() { return Err("closed".into())); }
    serde_json::from_str(buf.trim()).map_err(|e| format!("parse: {e}"))
}
```

Hmm but `Pin::new(&mut self.stream).split()` requires `Unpin`... `UnixStream` is `Unpin` so it should work.

Actually the simplest approach: keep a separate BufReader and use stream.write for writing:

```rust
pub struct ManagerConn {
    stream: UnixStream,
    reader: tokio::io::BufReader<tokio::net::unix::ReadHalf>,
}

impl ManagerConn {
    pub async fn connect() -> Result<Self, String> {
        let sock_path = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock_path).await
            .map_err(|e| format!("Cannot connect to Manager at {}: {e}", sock_path.display()))?;
        let (r, w) = stream.into_split();
        // Keep w for ctrl, put r into BufReader
        // We need both halves available
        let reader = BufReader::new(r);
        Ok(Self { stream: w.into(), reader })
    }
}
```

No wait. `into_split` consumes the stream and returns `(ReadHalf, WriteHalf)`. The WriteHalf can be used directly for writing. The ReadHalf goes into BufReader. This is the cleanest:

```rust
use tokio::net::unix::{ReadHalf, WriteHalf};

pub struct ManagerConn {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    buf: String,
}

impl ManagerConn {
    pub async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await
            .map_err(|e| format!("Cannot connect to Manager at {}: {e}", sock.display()))?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            buf: String::new(),
        })
    }

    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        let req = serde_json::json!({"method":"get_overview","id":"poll"});
        self.request(&req).await
    }

    pub async fn send_prompt(&mut self, session: &str, text: &str) -> Result<(), String> {
        let req = serde_json::json!({
            "method": "send", "session": session,
            "rpc_method": "prompt",
            "params": {"text": text}
        });
        self.request(&req).await?;
        Ok(())
    }

    pub async fn subscribe_session(&mut self, session: &str) -> Result<mpsc::UnboundedReceiver<ManagerEvent>, String> {
        let req = serde_json::json!({"method": "subscribe", "session": session, "id": "tui-sub"});
        let line = format!("{req}\n");
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        self.writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        // read ack
        self.buf.clear();
        self.reader.read_line(&mut self.buf).await.map_err(|e| format!("read: {e}"))?;
        if self.buf.is_empty() { return Err("closed".into())); }
        let ack: Value = serde_json::from_str(self.buf.trim()).map_err(|e| format!("parse: {e}"))?;
        if ack.get("type").and_then(|v| v.as_str()) != Some("subscribed") {
            return Err(format!("subscribe failed: {:?}", ack));
        }
        // Now spawn a reader task for this specific session
        let (tx, rx) = mpsc::unbounded_channel();
        // Problem: we can't have two readers on the same socket
        Err("subscribe_session requires separate socket connection".into())
    }

    async fn request(&mut self, req: &Value) -> Result<Value, String> {
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        self.writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        self.buf.clear();
        self.reader.read_line(&mut self.buf).await.map_err(|e| format!("read: {e}"))?;
        if self.buf.is_empty() { return Err("closed".into())); }
        serde_json::from_str(self.buf.trim()).map_err(|e| format!("parse: {e}"))
    }
}
```

The issue with `subscribe_session`: you can't have one reader reading both the request responses and the subscribe stream concurrently. Solution: **use separate socket connections for subscribe**. This is a common pattern:

- One "control" connection for request/response (poll_overview, send)
- One "event" connection per subscribe

This matches the architecture I described. Let me just go with simple connect-per-request for control, and a separate event loop connection for subscribe. Or even simpler: just use poll_overview (no real-time events) for MVP, and don't implement subscribe_session yet. The user asked for "快速开发."

For MVP:
- ManagerConn uses a single connection (reconnect per request, or keep-alive)
- poll_overview() at 1-2Hz via tick
- send_prompt() via same connection
- No subscribe_session in MVP

This is the fastest path to a working TUI. subscribe_session can come later for real-time output streaming.

Let me finalize the plan with this pragmatic approach. I've been overthinking this in the plan — let me just write clean code.

OK let me stop overthinking in the plan and just write the final plan for manager_conn as a pragmatic MVP.<｜end▁of▁thinking｜>I have all the context. Let me write the plan file now.

<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="Write">
<｜｜DSML｜｜parameter name="content" string="true"># ION TUI Dashboard 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建 `ion dashboard` TUI 仪表板，在一个终端面板中实时监控多 Worker 运行状态、查看输出、发送聊天，带赛博朋克霓虹主题和响应式布局。

**Architecture:**
- 内核补丁（WorkerRecord 状态流转 + subscribe_overview 事件流）为 TUI 提供准确数据
- `src/tui/` 模块用 ratatui 0.29 + crossterm 0.28 实现，通过 Unix socket 连 Manager
- MVP 用轮询（poll_overview 1Hz）获取数据，后续可升级为 subscribe_overview 流接收
- 事件循环：tick 驱动轮询 + 终端事件 + Manager 连接管理 → `AppState` → ratatui 渲染
- 响应式分栏：Wide(≥140) 三栏 / Medium(80-139) 两栏 / Narrow(<80) 单栏

**Tech Stack:** Rust, ratatui 0.29, crossterm 0.28, tokio, serde_json

---

### 文件结构

**新文件（12 个）：**

| 文件 | 职责 |
|------|------|
| `src/tui/mod.rs` | `pub fn run_dashboard()` 入口 |
| `src/tui/app.rs` | `AppState` + 三路事件循环（tick / 终端 / Manager 连接） |
| `src/tui/manager_conn.rs` | Unix socket 客户端（poll_overview / send_prompt） |
| `src/tui/theme.rs` | 赛博朋克调色板 + Style 常量 |
| `src/tui/layout.rs` | 响应式分栏计算 |
| `src/tui/state.rs` | 草稿/折叠状态持久化 |
| `src/tui/view/mod.rs` | render() 分发 |
| `src/tui/view/tree.rs` | 项目树 widget |
| `src/tui/view/kanban.rs` | 卡片网格 widget |
| `src/tui/view/detail.rs` | 详情/聊天面板 |
| `src/tui/view/input_box.rs` | 聊天输入框 widget |
| `src/tui/view/status_bar.rs` | 底部状态栏 |

**修改文件（3 个）：**

| 文件 | 改动 |
|------|------|
| `src/worker_registry.rs` | 状态流转（Busy → Idle on agent_end）+ latest_output 缓冲 |
| `src/bin/ion.rs` | + Dashboard 子命令 + subscribe_overview 命令 |
| `src/lib.rs` | + `pub mod tui` |
| `Cargo.toml` | + ratatui, crossterm |

---

### Task 0.1: WorkerRecord 状态流转 + latest_output 缓冲

**Files:**
- Modify: `src/worker_registry.rs:26-51` (WorkerRecord 新增字段)
- Modify: `src/worker_registry.rs:580-602` (send_command 设 Busy)
- Modify: `src/worker_registry.rs:1201-1285` (read_worker_stdout 检测 agent_end → Idle + 收集 text_delta)

- [ ] **Step 1: WorkerRecord 加 latest_output 和 model_size 字段**

在 WorkerRecord 结构体的 worktree 字段前插入：

```rust
pub latest_output: VecDeque<String>,  // 最近 5 行（非空文本段）
pub log_short: Option<String>,        // 最新一段的截断摘要（≤60 字符）
pub model_size: Option<String>,       // 模型上下文大小描述 "128k"
```

- [ ] **Step 2: create_worker 里初始化新字段**

```rust
// 在 create_worker 返回前，WorkerRecord 构建处添加：
latest_output: VecDeque::with_capacity(5),
log_short: None,
model_size: Some("128k".to_string()), // 默认值，后续从 session 加载
```

- [ ] **Step 3: send_command 里设 Busy**

```rust
// send_command 末尾（stdin.write_all / flush 成功后，Ok(()) 前）：
record.status = WorkerStatus::Busy;
```

- [ ] **Step 4: read_worker_stdout 里检测 agent_end → Idle + 收集 output**

在 `read_worker_stdout` 的 JSON 解析大 match 末尾（~line 1273-1277），修改现有逻辑：

```rust
// 在最后一个分支 (msg_type == "event") 判断具体事件类型
// 找到匹配 event 类型的地方，加 text_delta 和 agent_end 处理

// 在已有代码中扩展：
// 当 msg_type == "event" 时
let ev_type = msg.get("event")
    .and_then(|e| e.get("type"))
    .and_then(|v| v.as_str())
    .unwrap_or("");

if ev_type == "agent_end" {
    record.status = WorkerStatus::Idle;
    // 广播 overview（另起 task，避免持锁 await）
    let reg_clone = Arc::clone(&registry);
    let wid = worker_id.clone();
    tokio::spawn(async move {
        let mut r = reg_clone.lock().await;
        r.broadcast_overview();
    });
}

if ev_type == "text_delta" {
    if let Some(delta) = msg.get("event")
        .and_then(|e| e.get("delta"))
        .and_then(|v| v.as_str())
    {
        record.latest_output.push_back(delta.to_string());
        while record.latest_output.len() > 5 {
            record.latest_output.pop_front();
        }
        // 更新 log_short（只取最新一段的前 60 字符）
        let short = delta.chars().take(60).collect::<String>();
        record.log_short = Some(short);
    }
}
```

- [ ] **Step 5: get_overview 输出 latest_output 等字段**

```rust
// get_overview() 的 workers.map 闭包里加字段：
"latest_output": w.latest_output.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
"log_short": w.log_short,
"model_size": w.model_size,
"started_at": w.started_at,
```

- [ ] **Step 6: 编译验证**

```bash
cargo build --bin ion
```

- [ ] **Step 7: Commit**

```bash
git add src/worker_registry.rs
git commit -m "feat(registry): add status transitions and latest_output buffer"
```

---

### Task 0.2: subscribe_overview 内核事件流

**Files:**
- Modify: `src/worker_registry.rs` (overview_subscribers + broadcast_overview + subscribe_overview)
- Modify: `src/bin/ion.rs` (socket handler 加 subscribe_overview 命令)

- [ ] **Step 1: WorkerRegistry 加 overview_subscribers 字段 + 初始化**

```rust
// 在 WorkerRegistry 结构体，global_subscribers 后面：
pub overview_subscribers: Vec<mpsc::UnboundedSender<serde_json::Value>>,
```

```rust
// WorkerRegistry::new() 里加：
overview_subscribers: Vec::new(),
```

- [ ] **Step 2: 实现 broadcast_overview 和 subscribe_overview**

```rust
// 在 WorkerRegistry impl 里：
pub fn broadcast_overview(&mut self) {
    let overview = self.get_overview();
    self.overview_subscribers.retain(|tx| {
        tx.send(overview.clone()).is_ok()
    });
}

pub fn subscribe_overview(&mut self) -> mpsc::UnboundedReceiver<serde_json::Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    self.overview_subscribers.push(tx);
    rx
}
```

- [ ] **Step 3: 在 create_worker / kill_worker 末尾广播**

```rust
// create_worker（emit_global 之后）：
self.broadcast_overview();

// kill_worker（emit_global 之后）：
self.broadcast_overview();
```

- [ ] **Step 4: socket handler 加 subscribe_overview 命令**

在 `src/bin/ion.rs` socket accept loop 里，在 `method == "subscribe"` 块后面（~line 1646）加：

```rust
if method == "subscribe_overview" {
    let (initial, rx) = {
        let mut reg = reg.lock().await;
        let overview = reg.get_overview();
        let rx = reg.subscribe_overview();
        (overview, rx)
    };
    // 返回初始快照
    let ack = serde_json::json!({
        "type": "response",
        "id": cmd.get("id"),
        "success": true,
        "data": {
            "stream": "overview",
            "initial": initial,
        }
    });
    let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
    let _ = write_half.flush().await;
    // 持续推后续变更
    let mut rx = rx;
    loop {
        match rx.recv().await {
            Some(snapshot) => {
                let msg = serde_json::json!({
                    "type": "overview_snapshot",
                    "data": snapshot,
                });
                if write_half.write_all(format!("{msg}\n").as_bytes()).await.is_err() {
                    break;
                }
                let _ = write_half.flush().await;
            }
            None => break,
        }
    }
    return;
}
```

- [ ] **Step 5: 编译验证**

```bash
cargo build --bin ion
```

测试：`ion serve start` 然后在另一个终端 `echo '{"method":"subscribe_overview","id":"test"}' | nc -U ~/.ion/host.sock`。

- [ ] **Step 6: Commit**

```bash
git add src/worker_registry.rs src/bin/ion.rs
git commit -m "feat(manager): add subscribe_overview streaming endpoint"
```

---

### Task 0.3: Manager 后台 heartbeat stale 检测

**Files:**
- Modify: `src/bin/ion.rs` (cmd_manager_start 后台任务)

- [ ] **Step 1: 加心跳超时检查 Task**

```rust
// 在 cmd_manager_start 后台任务 3（转发全局事件）后面加：
// 后台任务 4：heartbeat 超时 → Stale
let hb_registry = Arc::clone(&registry);
tokio::spawn(async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        let mut reg = hb_registry.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let mut changed = false;
        for record in reg.workers.values_mut() {
            if record.status != WorkerStatus::Dead && record.status != WorkerStatus::Stale {
                if now - record.last_heartbeat > 180_000 { // 3 分钟无心跳
                    record.status = WorkerStatus::Stale;
                    changed = true;
                }
            }
        }
        if changed {
            reg.broadcast_overview();
        }
    }
});
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/bin/ion.rs && git commit -m "feat(manager): heartbeat stale detection"
```

---

### Task 1.1: Cargo.toml + lib.rs 准备

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`

- [ ] **Step 1: Cargo.toml 加 ratatui + crossterm**

```toml
ratatui = "0.29"
crossterm = "0.28"
```

- [ ] **Step 2: lib.rs 加模块**

```rust
pub mod tui;
```

- [ ] **Step 3: 编译 + commit**

```bash
cargo build --bin ion
git add Cargo.toml Cargo.lock src/lib.rs
git commit -m "feat(tui): add ratatui/crossterm deps and mod declaration"
```

---

### Task 1.2: src/tui/mod.rs + theme.rs — 入口和主题

**Files:**
- Create: `src/tui/mod.rs`
- Create: `src/tui/theme.rs`

- [ ] **Step 1: theme.rs — 赛博朋克调色板**

```rust
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Color,
    pub panel_bg: Color,
    pub text: Color,
    pub subtext: Color,
    pub accent: Color,
    pub danger: Color,
    pub warning: Color,
    pub dead: Color,
    pub border_focused: Color,
    pub border_normal: Color,
    pub border_inactive: Color,
}

impl Theme {
    pub const fn cyberpunk() -> Self {
        Self {
            bg: Color::Rgb(0x0a, 0x0e, 0x1a),
            panel_bg: Color::Rgb(0x11, 0x18, 0x27),
            text: Color::Rgb(0xc8, 0xd3, 0xf5),
            subtext: Color::Rgb(0x5b, 0x6b, 0x9c),
            accent: Color::Rgb(0x00, 0xff, 0xd1),
            danger: Color::Rgb(0xff, 0x2d, 0x95),
            warning: Color::Rgb(0xff, 0xb8, 0x00),
            dead: Color::Rgb(0x7a, 0x1f, 0x3d),
            border_focused: Color::Rgb(0x00, 0xff, 0xd1),
            border_normal: Color::Rgb(0x1f, 0x4d, 0x5c),
            border_inactive: Color::Rgb(0x2a, 0x33, 0x49),
        }
    }
}

/// 状态对应的显示字符和颜色
pub fn status_icon(status: &str) -> (&'static str, Style) {
    let theme = Theme::cyberpunk();
    match status {
        "busy"  => ("▶", Style::default().fg(theme.danger).add_modifier(Modifier::BOLD)),
        "idle"  => ("⏸", Style::default().fg(theme.accent)),
        "stale" => ("⚠", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        "dead"  => ("⨯", Style::default().fg(theme.dead)),
        _       => ("?", Style::default().fg(theme.subtext)),
    }
}
```

- [ ] **Step 2: mod.rs — 模块声明 + 入口函数**

```rust
pub mod app;
pub mod layout;
pub mod manager_conn;
pub mod theme;
pub mod state;
pub mod view;

/// 启动 Dashboard TUI
pub async fn run_dashboard() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 连接 Manager
    let mut conn = manager_conn::ManagerConn::connect().await
        .map_err(|e| format!("Cannot connect to Manager: {e}"))?;

    // 2. 取初始 overview
    let overview = conn.poll_overview().await
        .map_err(|e| format!("Failed to get overview: {e}"))?;

    // 3. 构建 AppState
    let mut app = app::AppState::new(conn, overview);

    // 4. 进入 TUI 事件循环
    app.run().await
}
```

- [ ] **Step 3: 编译 + commit**

```bash
cargo build --bin ion
git add src/tui/mod.rs src/tui/theme.rs
git commit -m "feat(tui): dashboard entry point and cyberpunk theme"
```

---

### Task 1.3: layout.rs — 响应式分栏

**Files:**
- Create: `src/tui/layout.rs`

- [ ] **Step 1: 布局类型 + 计算**

```rust
use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutTier { Wide, Medium, Narrow }

#[derive(Debug)]
pub struct AppLayout {
    pub tier: LayoutTier,
    pub tree_rect: Option<Rect>,
    pub kanban_rect: Option<Rect>,
    pub detail_rect: Option<Rect>,
    pub status_bar_rect: Rect,
}

impl LayoutTier {
    pub fn from_width(width: u16) -> Self {
        if width >= 140 { Self::Wide }
        else if width >= 80 { Self::Medium }
        else { Self::Narrow }
    }
}

pub fn compute_layout(area: Rect, tier: LayoutTier, focus_mode: bool) -> AppLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(1)])
        .split(area);
    let main = chunks[0];
    let status_bar_rect = chunks[1];

    if focus_mode {
        return AppLayout {
            tier, tree_rect: None, kanban_rect: None,
            detail_rect: Some(main), status_bar_rect,
        };
    }

    match tier {
        LayoutTier::Wide => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(28),
                    Constraint::Fill(1),
                    Constraint::Length(40),
                ])
                .split(main);
            AppLayout {
                tier,
                tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: Some(cols[2]),
                status_bar_rect,
            }
        }
        LayoutTier::Medium => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Fill(1)])
                .split(main);
            AppLayout {
                tier, tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: None, status_bar_rect,
            }
        }
        LayoutTier::Narrow => {
            AppLayout {
                tier, tree_rect: None,
                kanban_rect: Some(main),
                detail_rect: None, status_bar_rect,
            }
        }
    }
}

pub fn cards_per_row(width: u16) -> usize {
    std::cmp::max(1, width.saturating_sub(4) as usize / 42)
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/layout.rs && git commit -m "feat(tui): responsive layout"
```

---

### Task 1.4: manager_conn.rs — 带自动重连的 socket 客户端

**Files:**
- Create: `src/tui/manager_conn.rs`

- [ ] **Step 1: 实现 ManagerConn**

```rust
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{ReadHalf, WriteHalf};

/// Manager socket 客户端
/// MVP 用单连接 + 轮询 poll_overview
/// 后续可加 subscribe_overview 流接收
pub struct ManagerConn {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    buf: String,
    connected: bool,
}

impl ManagerConn {
    pub async fn connect() -> Result<Self, String> {
        let sock = crate::paths::manager_socket_path();
        let stream = UnixStream::connect(&sock).await
            .map_err(|e| format!("Cannot connect to Manager at {}: {e}", sock.display()))?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            buf: String::new(),
            connected: true,
        })
    }

    pub fn is_connected(&self) -> bool { self.connected }

    /// 轮询 overview
    pub async fn poll_overview(&mut self) -> Result<Value, String> {
        self.request(&serde_json::json!({"method":"get_overview","id":"poll"})).await
    }

    /// 发送聊天消息到指定 session
    pub async fn send_prompt(&mut self, session: &str, text: &str) -> Result<Value, String> {
        let req = serde_json::json!({
            "method": "send", "session": session,
            "rpc_method": "prompt",
            "params": {"text": text}
        });
        self.request(&req).await
    }

    /// 底层读写
    async fn request(&mut self, req: &Value) -> Result<Value, String> {
        if !self.connected {
            // 断线重连
            *self = Self::connect().await?;
        }
        let line = format!("{req}\n");
        self.writer.write_all(line.as_bytes()).await
            .map_err(|e| { self.connected = false; format!("write: {e}") })?;
        self.writer.flush().await
            .map_err(|e| { self.connected = false; format!("flush: {e}") })?;
        self.buf.clear();
        match self.reader.read_line(&mut self.buf).await {
            Ok(0) => { self.connected = false; Err("connection closed".into()) }
            Ok(_) => serde_json::from_str(self.buf.trim())
                .map_err(|e| format!("parse: {e}")),
            Err(e) => { self.connected = false; Err(format!("read: {e}")) }
        }
    }
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/manager_conn.rs && git commit -m "feat(tui): ManagerUnix socket client"
```

---

### Task 1.5: state.rs — 草稿/折叠状态持久化

**Files:**
- Create: `src/tui/state.rs`

- [ ] **Step 1: 持久化结构体**

```rust
use std::collections::{HashMap, HashSet};

/// 持久化的 TUI 状态（~/.ion/tui-state.json）
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct TuiState {
    pub collapsed_projects: HashSet<String>,
    pub drafts: HashMap<String, String>,
    pub last_selected_session: Option<String>,
}

static STATE_PATH: &str = "tui-state.json";

fn state_path() -> std::path::PathBuf {
    let base = crate::paths::ion_dir();
    base.join(STATE_PATH)
}

impl TuiState {
    pub fn load() -> Self {
        let path = state_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(state_path(), s);
        }
    }
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/state.rs && git commit -m "feat(tui): TUI state persistence"
```

---

### Task 1.6: app.rs — AppState + 事件循环（TUI 心脏）

**Files:**
- Create: `src/tui/app.rs`

- [ ] **Step 1: AppState 定义**

```rust
use std::collections::{HashMap, HashSet, VecDeque};
use ratatui::{
    Terminal, backend::{CrosstermBackend, Backend},
    layout::Rect,
};
use crossterm::event::{self as crossterm_event, Event, KeyEventKind};
use serde_json::Value;
use tokio::time::{interval, Duration};

use crate::tui::{
    manager_conn::ManagerConn,
    layout::{self, LayoutTier, AppLayout},
    theme::{self, Theme},
    view,
    state::TuiState,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeId {
    Project(String),
    Session(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel { Tree, Kanban, Detail, Input }

pub struct AppState {
    // ── 数据 ──
    pub workers: Vec<Value>,         // 当前 overview workers 列表
    pub projects: Vec<Value>,        // 当前 overview projects 列表
    pub total_workers: usize,
    pub total_projects: usize,

    // ── 状态 ──
    pub selected: Option<NodeId>,
    pub focused_panel: Panel,
    pub focus_mode: bool,
    pub collapsed: HashSet<String>,
    pub drafts: HashMap<String, String>,
    pub log_buffers: HashMap<String, VecDeque<String>>,

    // ── 草稿输入 ──
    pub input_text: String,
    pub input_cursor: usize,

    // ── 连接 ──
    pub conn: ManagerConn,
    pub connected: bool,

    // ── 布局 ──
    pub layout_tier: LayoutTier,
    pub term_width: u16,
    pub term_height: u16,

    // ── 持久化 ──
    pub tui_state: TuiState,

    // ── 选中 session（用于发送聊天） ──
    pub active_session: Option<String>,

    // ── 退出信号 ──
    pub should_quit: bool,
}
```

- [ ] **Step 2: AppState::new + run**

```rust
impl AppState {
    pub fn new(conn: ManagerConn, overview: Value) -> Self {
        let mut st = Self::from_overview(Self {
            workers: vec![],
            projects: vec![],
            total_workers: 0,
            total_projects: 0,
            selected: None,
            focused_panel: Panel::Tree,
            focus_mode: false,
            collapsed: HashSet::new(),
            drafts: HashMap::new(),
            log_buffers: HashMap::new(),
            input_text: String::new(),
            input_cursor: 0,
            conn,
            connected: true,
            layout_tier: LayoutTier::Wide,
            term_width: 140,
            term_height: 40,
            tui_state: TuiState::load(),
            active_session: None,
            should_quit: false,
        });
        st.apply_overview(overview);
        // 恢复折叠状态
        st.collapsed = st.tui_state.collapsed_projects.clone();
        // 恢复草稿
        st.drafts = st.tui_state.drafts.clone();
        st
    }

    fn from_overview(&self) -> Self { /* 占位，实际已在 new 里完成 */ }

    fn apply_overview(&mut self, data: Value) {
        if let Some(workers) = data.get("workers").and_then(|v| v.as_array()) {
            self.workers = workers.clone();
            self.total_workers = workers.len();
        }
        if let Some(projects) = data.get("projects").and_then(|v| v.as_array()) {
            self.projects = projects.clone();
            self.total_projects = projects.len();
        }
        if let Some(total) = data.get("total_workers").and_then(|v| v.as_u64()) {
            self.total_workers = total as usize;
        }
        if let Some(total) = data.get("total_projects").and_then(|v| v.as_u64()) {
            self.total_projects = total as usize;
        }
    }
}
```

- [ ] **Step 3: run() 事件循环 — 三路 select!**

```rust
impl AppState {
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use crossterm::terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
        use crossterm::execute;
        use std::io::stdout;

        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

        // 确保退出时恢复终端
        let result = self.run_inner(&mut terminal).await;
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);

        // 保存状态
        self.tui_state.collapsed_projects = self.collapsed.clone();
        self.tui_state.drafts = self.drafts.clone();
        self.tui_state.save();

        result
    }

    async fn run_inner<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), Box<dyn std::error::Error>> {
        let mut tick = interval(Duration::from_millis(250));
        loop {
            // 1. Tick 事件（轮询 + 动画）
            tokio::select! {
                _ = tick.tick() => {
                    self.on_tick().await;
                }
                event = tokio::task::spawn_blocking(|| {
                    crossterm_event::read()
                }) => {
                    match event {
                        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                            self.on_key(key);
                        }
                        Ok(Event::Resize(w, h)) => {
                            self.term_width = w;
                            self.term_height = h;
                            self.layout_tier = LayoutTier::from_width(w);
                        }
                        _ => {}
                    }
                }
            }

            if self.should_quit { break; }

            // 2. 渲染
            terminal.draw(|f| {
                let layout = layout::compute_layout(f.area(), self.layout_tier, self.focus_mode);
                view::render(f, self, &layout);
            })?;
        }
        Ok(())
    }
}
```

- [ ] **Step 4: on_tick + on_key 骨架**

```rust
impl AppState {
    async fn on_tick(&mut self) {
        // 轮询 overview
        if let Ok(data) = self.conn.poll_overview().await {
            self.connected = true;
            let prev_count = self.workers.len();
            self.apply_overview(data);
            // 新 worker 出现 → 还原草稿
            if self.workers.len() > prev_count {
                for w in &self.workers {
                    if let Some(sid) = w.get("session_id").and_then(|v| v.as_str()) {
                        if !self.log_buffers.contains_key(sid) {
                            self.log_buffers.insert(sid.to_string(), VecDeque::with_capacity(20));
                        }
                    }
                }
            }
        } else {
            self.connected = false;
        }
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('q') => { self.should_quit = true; }
            KeyCode::Char('z') => { self.toggle_collapse(); }
            KeyCode::Tab => { self.focus_next_panel(); }
            KeyCode::BackTab => { self.focus_prev_panel(); }
            KeyCode::Char('d') => { self.toggle_focus_mode(); }
            KeyCode::Esc => { self.focus_mode = false; }
            KeyCode::Enter => { self.on_enter(); }
            KeyCode::Up | KeyCode::Char('k') => { self.navigate_up(); }
            KeyCode::Down | KeyCode::Char('j') => { self.navigate_down(); }
            KeyCode::Char('/') => { /* 搜索 — 跳过 MVP */ }
            _ => {
                if self.focused_panel == Panel::Input {
                    self.handle_input(key);
                }
            }
        }
    }
}
```

- [ ] **Step 5: 辅助方法骨架**

```rust
impl AppState {
    fn toggle_collapse(&mut self) {
        if let Some(NodeId::Project(name)) = &self.selected.clone() {
            if !self.collapsed.remove(name) {
                self.collapsed.insert(name.clone());
            }
        }
    }

    fn focus_next_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Tree => Panel::Kanban,
            Panel::Kanban if self.focus_mode => Panel::Detail,
            Panel::Kanban => Panel::Tree,
            Panel::Detail => Panel::Input,
            Panel::Input => Panel::Tree,
            _ => Panel::Tree,
        };
    }

    fn focus_prev_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            Panel::Tree => Panel::Input,
            Panel::Kanban => Panel::Tree,
            Panel::Detail => Panel::Kanban,
            Panel::Input => Panel::Detail,
            _ => Panel::Tree,
        };
    }

    fn toggle_focus_mode(&mut self) {
        self.focus_mode = !self.focus_mode;
    }

    fn on_enter(&mut self) {
        // 选中当前选中的卡片
        if self.focused_panel == Panel::Kanban {
            if let Some(NodeId::Session(sid)) = &self.selected.clone() {
                self.active_session = Some(sid.clone());
                self.focus_mode = true;
                self.focused_panel = Panel::Detail;
            }
        }
    }

    fn navigate_up(&mut self) { /* 选中上一个 */ }
    fn navigate_down(&mut self) { /* 选中下一个 */ }

    fn handle_input(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char(c) => {
                self.input_text.insert(self.input_cursor, c);
                self.input_cursor += 1;
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    self.input_cursor -= 1;
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input_text.len() {
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Left => { self.input_cursor = self.input_cursor.saturating_sub(1); }
            KeyCode::Right => { self.input_cursor = self.input_cursor.min(self.input_text.len()); }
            KeyCode::Enter => {
                // 特殊处理 ctrl+enter 发送
            }
            _ => {}
        }
    }
}
```

- [ ] **Step 6: 编译（暂时注释掉未实现的 view::render 引用）**

```bash
cargo build --bin ion
```

- [ ] **Step 7: Commit**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): AppState and event loop skeleton (triple select!)"
```

---

### Task 1.7: view/mod.rs — 渲染分发

**Files:**
- Create: `src/tui/view/mod.rs`

- [ ] **Step 1: 模块声明 + render 主函数**

```rust
pub mod tree;
pub mod kanban;
pub mod detail;
pub mod input_box;
pub mod status_bar;

use ratatui::{Frame, layout::Rect, backend::Backend};
use crate::tui::app::AppState;

pub fn render<B: Backend>(f: &mut Frame<B>, state: &mut AppState, layout: &crate::tui::layout::AppLayout) {
    let theme = crate::tui::theme::Theme::cyberpunk();

    // 背景填充
    let bg_style = ratatui::style::Style::default().bg(theme.bg);
    f.render_widget(ratatui::widgets::Clear, f.area());
    f.render_widget(ratatui::widgets::Block::default().style(bg_style), f.area());

    // 渲染各面板
    if let Some(rect) = layout.tree_rect {
        tree::render(f, state, rect, theme, state.focused_panel == crate::tui::app::Panel::Tree);
    }
    if let Some(rect) = layout.kanban_rect {
        kanban::render(f, state, rect, theme, state.focused_panel == crate::tui::app::Panel::Kanban);
    }
    if let Some(rect) = layout.detail_rect {
        detail::render(f, state, rect, theme, state.focused_panel == crate::tui::app::Panel::Detail);
    }

    // 输入框（全部模式都在底部固定区域渲染，或 Kanban 模式下覆盖）
    if state.focus_mode || state.focused_panel == Panel::Input {
        // 在 detail_rect 底部划出一块输入区域
    }

    // 状态栏
    status_bar::render(f, state, layout.status_bar_rect, theme);
}
```

- [ ] **Step 2: 编译（子模块空文件）**

```bash
cargo build --bin ion
```

先创建空文件跳过编译错误：
```bash
mkdir -p src/tui/view
touch src/tui/view/tree.rs
touch src/tui/view/kanban.rs
touch src/tui/view/detail.rs
touch src/tui/view/input_box.rs
touch src/tui/view/status_bar.rs
```

然后让每个空文件中有一个 pub fn render dummy。

- [ ] **Step 3: Commit**

```bash
git add src/tui/view/
git commit -m "feat(tui): render dispatcher with panel stubs"
```

---

### Task 1.8: view/tree.rs — 项目树

**Files:**
- Create: `src/tui/view/tree.rs`

- [ ] **Step 1: 项目树渲染**

```rust
use ratatui::{
    Frame, backend::Backend, layout::Rect,
    widgets::{Block, Borders, List, ListItem, ListState},
    style::{Style, Modifier},
    text::Text,
};
use crate::tui::{app::AppState, app::NodeId, theme::Theme};

pub fn render<B: Backend>(f: &mut Frame<B>, state: &mut AppState, area: Rect, theme: Theme, focused: bool) {
    let border_color = if focused { theme.border_focused } else { theme.border_inactive };
    let block = Block::default()
        .title(format!(" Projects · {} ", state.total_projects))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel_bg));

    let mut items: Vec<ListItem> = Vec::new();
    for proj in &state.projects {
        let name = proj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let count = proj.get("worker_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let collapsed = state.collapsed.contains(name);
        let icon = if collapsed { "▶" } else { "▼" };
        let total_str = format!(" ({count})");

        // 项目行
        let line = format!(" {} {} {}", icon, name, total_str);
        let style = if focused && state.selected == Some(NodeId::Project(name.to_string())) {
            Style::default().fg(theme.accent).bg(Color::Rgb(0x1a, 0x2a, 0x3a))
        } else {
            Style::default().fg(theme.text)
        };
        items.push(ListItem::new(Text::styled(line, style)));

        // 项目下的会话（如果未折叠）
        if !collapsed {
            for w in &state.workers {
                let proj_name = w.get("project").and_then(|v| v.as_str()).unwrap_or("");
                if proj_name != name { continue; }
                let sid = w.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
                let agent = w.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
                let (icon, st) = theme::status_icon(
                    w.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                );
                let short_sid = if sid.len() > 8 { &sid[..8] } else { sid };
                let line = format!(" {} {} {}", icon, short_sid, agent);
                let mut s = st;
                if focused && state.selected == Some(NodeId::Session(sid.to_string())) {
                    s = s.bg(Color::Rgb(0x1a, 0x2a, 0x3a));
                }
                items.push(ListItem::new(Text::styled(format!("  {}", line), s)));
            }
        }
    }

    let list = List::new(items).block(block).highlight_style(
        Style::default().add_modifier(Modifier::REVERSED)
    );
    f.render_widget(list, area);
}
```

注意这里和 AppState 的交互——需要添加 `get_selected_index` 导航方法并用 `ListState` 记录选中索引。MVP 简化：不实现滚动 `ListState`，用纯文字渲染 + `selected` 高亮。

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/view/tree.rs && git commit -m "feat(tui): project tree widget"
```

---

### Task 1.9: view/kanban.rs — 卡片网格

**Files:**
- Create: `src/tui/view/kanban.rs`

- [ ] **Step 1: 卡片渲染**

```rust
use ratatui::{
    Frame, backend::Backend, layout::{Rect, Layout, Constraint, Direction, Alignment},
    widgets::{Block, Borders, Paragraph, Wrap, Gauge},
    style::{Style, Modifier, Color},
    text::{Text, Span, Line},
};
use crate::tui::{app::AppState, app::NodeId, theme, theme::Theme, layout};

pub fn render<B: Backend>(f: &mut Frame<B>, state: &mut AppState, area: Rect, theme: Theme, focused: bool) {
    let cols = layout::cards_per_row(area.width);
    if cols == 0 || state.workers.is_empty() {
        let block = Block::default()
            .title(" Workers · 0 ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused { theme.border_focused } else { theme.border_inactive }))
            .style(Style::default().bg(theme.panel_bg));
        f.render_widget(Paragraph::new("No active workers").block(block), area);
        return;
    }

    // 卡片的列宽分配
    let card_width = area.width / cols as u16;
    let rows = (state.workers.len() + cols - 1) / cols;

    for (i, w) in state.workers.iter().enumerate() {
        let row = i / cols;
        let col = i % cols;
        let x = area.x + col as u16 * card_width;
        let y = area.y + row as u16 * 8; // 每卡片高 8 行
        if y + 8 > area.y + area.height { break; }
        let card_rect = Rect::new(x, y, card_width, 8);

        let sid = w.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
        let agent = w.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
        let model = w.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let status = w.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let log = w.get("log_short").and_then(|v| v.as_str()).unwrap_or("");
        let started = w.get("started_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let uptime = if started > 0 {
            let secs = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64 - started) / 1000;
            format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        } else { "--:--:--".into() };

        let (icon, st) = theme::status_icon(status);
        let selected = focused && state.selected == Some(NodeId::Session(sid.to_string()));
        let border_color = if selected { theme.accent } else if focused { theme.border_focused } else { theme.border_inactive };

        // 卡片边框 + 状态色条（左 2px）
        let lines = vec![
            Line::from(vec![
                Span::styled(format!("{} {} ", icon, agent), st),
                Span::styled(format!("· {} ", model), Style::default().fg(theme.subtext)),
            ]),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(uptime, Style::default().fg(theme.subtext)),
                Span::raw(" · "),
                Span::styled(status.to_uppercase(), st),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    if log.len() > 50 { format!("{}...", &log[..50]) } else { log.to_string() },
                    Style::default().fg(theme.text),
                ),
            ]),
        ];

        let para = Paragraph::new(lines)
            .block(Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)));
        f.render_widget(para, card_rect);
    }

    // 标题块覆盖整个区域但透明
    let header = Block::default()
        .title(format!(" Workers · {} ", state.total_workers))
        .borders(Borders::NONE)
        .style(Style::default().bg(Color::Reset));
    f.render_widget(header, area);
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/view/kanban.rs && git commit -m "feat(tui): kanban card grid widget"
```

---

### Task 1.10: view/detail.rs — 详情/Focus 模式

**Files:**
- Create: `src/tui/view/detail.rs`

- [ ] **Step 1: 详情面板渲染**

```rust
use ratatui::{
    Frame, backend::Backend, layout::{Rect, Layout, Constraint, Direction},
    widgets::{Block, Borders, Paragraph, Wrap},
    style::{Style, Modifier, Color},
    text::{Text, Span, Line},
};
use crate::tui::{app::AppState, app::Panel, theme::Theme};

pub fn render<B: Backend>(f: &mut Frame<B>, state: &AppState, area: Rect, theme: Theme, focused: bool) {
    let session_id = match &state.active_session {
        Some(sid) => sid.clone(),
        None => {
            let p = Paragraph::new("Select a worker card and press Enter")
                .block(Block::default()
                    .title(" Detail ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border_inactive))
                    .style(Style::default().bg(theme.panel_bg)))
                .style(Style::default().fg(theme.subtext));
            f.render_widget(p, area);
            return;
        }
    };

    // 找 worker 信息
    let worker = state.workers.iter().find(|w|
        w.get("session_id").and_then(|v| v.as_str()) == Some(&session_id)
    );

    let border_color = if focused { theme.border_focused } else { theme.border_inactive };

    if state.focus_mode {
        // Focus 模式：detail + input（下方）+ side panels（右侧）
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1), Constraint::Length(24)])
            .split(area);
        let main = chunks[0];
        let side = chunks[1];

        // 主聊天区域
        let chat = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(3)])
            .split(main);

        // 聊天内容占位
        let agent = worker.and_then(|w| w.get("agent").and_then(|v| v.as_str())).unwrap_or("?");
        let model = worker.and_then(|w| w.get("model").and_then(|v| v.as_str())).unwrap_or("?");
        let title = format!(" {} · {} ({}) ", session_id.chars().take(8).collect::<String>(), agent, model);
        let content = Paragraph::new(render_chat_content(state, &session_id))
            .block(Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)))
            .wrap(Wrap { trim: false });
        f.render_widget(content, chat[0]);

        // 输入框
        let input_block = Block::default()
            .title(" Input ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(
                if state.focused_panel == Panel::Input { theme.border_focused } else { theme.border_inactive }
            ))
            .style(Style::default().bg(theme.panel_bg));
        let input_para = Paragraph::new(state.input_text.as_str())
            .block(input_block)
            .style(Style::default().fg(theme.text));
        f.render_widget(input_para, chat[1]);

        // 侧栏 — Todo 面板占位
        let todo_block = Block::default()
            .title(" Todo ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_inactive))
            .style(Style::default().bg(theme.panel_bg));
        let todo_para = Paragraph::new("(todo plugin)")
            .block(todo_block)
            .style(Style::default().fg(theme.subtext));
        f.render_widget(todo_para, side);
    } else {
        // 非 Focus 模式：精简版 detail（Kanban 时右侧显示）
        let agent = worker.and_then(|w| w.get("agent").and_then(|v| v.as_str())).unwrap_or("?");
        let model = worker.and_then(|w| w.get("model").and_then(|v| v.as_str())).unwrap_or("?");
        let log = worker.and_then(|w| w.get("log_short").and_then(|v| v.as_str())).unwrap_or("No output yet");
        let lines = vec![
            Line::from(vec![Span::styled(agent, Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))]),
            Line::from(vec![Span::styled(model, Style::default().fg(theme.subtext))]),
            Line::from(""),
            Line::from(vec![Span::styled(log, Style::default().fg(theme.text))]),
        ];
        let p = Paragraph::new(lines)
            .block(Block::default()
                .title(" Detail ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }
}

fn render_chat_content(state: &AppState, session_id: &str) -> Text {
    // 从 log_buffers 收集输出
    let mut lines = Vec::new();
    if let Some(buf) = state.log_buffers.get(session_id) {
        for l in buf.iter() {
            lines.push(Line::from(Span::raw(l.as_str())));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::raw("(waiting for output...)")));
    }
    Text::from(lines)
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/view/detail.rs && git commit -m "feat(tui): detail/focus mode panel"
```

---

### Task 1.11: view/input_box.rs — 输入框 widget（可独立组件化）

**Files:**
- Create: `src/tui/view/input_box.rs`

- [ ] **Step 1: 输入框渲染（复用 detail 内 input，这里也暴露独立函数）**

```rust
use ratatui::{
    Frame, backend::Backend, layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::Style,
};
use crate::tui::{app::AppState, theme::Theme};

pub fn render<B: Backend>(f: &mut Frame<B>, state: &AppState, area: Rect, theme: Theme, focused: bool) {
    let border_color = if focused { theme.border_focused } else { theme.border_inactive };
    let block = Block::default()
        .title(" Input ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel_bg));
    let para = Paragraph::new(state.input_text.as_str())
        .block(block)
        .style(Style::default().fg(theme.text));
    f.render_widget(para, area);
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/view/input_box.rs && git commit -m "feat(tui): input box widget"
```

---

### Task 1.12: view/status_bar.rs — 底部状态栏

**Files:**
- Create: `src/tui/view/status_bar.rs`

- [ ] **Step 1: 状态栏渲染**

```rust
use ratatui::{
    Frame, backend::Backend, layout::Rect,
    widgets::Paragraph,
    style::{Style, Modifier},
    text::{Span, Line},
};
use crate::tui::{app::AppState, theme::Theme};

pub fn render<B: Backend>(f: &mut Frame<B>, state: &AppState, area: Rect, theme: Theme) {
    let conn_style = if state.connected {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.dead)
    };
    let conn = if state.connected { "● connected" } else { "○ disconnected" };

    let line = Line::from(vec![
        Span::styled(conn, conn_style),
        Span::raw("  "),
        Span::styled(
            format!("{} workers | {} projects | {} live",
                state.total_workers, state.total_projects,
                state.workers.iter().filter(|w| {
                    w.get("status").and_then(|v| v.as_str()) != Some("dead")
                }).count()),
            Style::default().fg(theme.subtext)),
        Span::raw("  "),
        Span::styled("Tab:switch Enter:select z:collapse d:focus /:search q:quit",
            Style::default().fg(theme.subtext)),
    ]);

    let p = Paragraph::new(line)
        .style(Style::default().bg(theme.bg).fg(theme.text));
    f.render_widget(p, area);
}
```

- [ ] **Step 2: 编译 + commit**

```bash
cargo build && git add src/tui/view/status_bar.rs && git commit -m "feat(tui): status bar widget"
```

---

### Task 1.13: ion.rs — 加 Dashboard 子命令

**Files:**
- Modify: `src/bin/ion.rs`

- [ ] **Step 1: Commands 枚举加 Dashboard 变体**

```rust
// 在 Commands 枚举里 List 后面或 Subscribe 前面加：
/// Launch the TUI dashboard
Dashboard,
```

- [ ] **Step 2: 主 match 添加 dispatch**

```rust
// 在 cmd_list_agents 调用前，或 Subscribe 前面：
Some(Commands::Dashboard) => {
    if let Err(e) = ion::tui::run_dashboard().await {
        eprintln!("Dashboard error: {e}");
    }
}
```

- [ ] **Step 3: 编译 + commit**

```bash
cargo build --bin ion && git add src/bin/ion.rs && git commit -m "feat(cli): add dashboard subcommand"
```

---

### Task 2.1: 全流程集成 — 修复编译错误 + 让 TUI 能显示

**Files:**
- Fix: 所有之前写的 TUI 文件（类型错误、引用缺失）

- [ ] **Step 1: 全面编译测试**

```bash
cargo build --bin ion
```

修复所有编译错误。已知可能需要修复的点：
- ratatui 0.29 API 差异（比如 `Borders` 可能叫 `border_type`）
- 缺失的 use 导入
- `app.rs` 里 `ManagerConn::connect()` 路径
- `view/mod.rs` 里缺失的 `Panel` use

- [ ] **Step 2: 启动 Manager+Worker 测试 TUI**

```bash
# 终端 1
ion serve start

# 终端 2
ion dashboard
```

验证能看到项目树和 worker 卡片。

- [ ] **Step 3: Commit**

```bash
git add src/tui/ && git commit -m "fix(tui): integration fixes and first working dashboard"
```

---

### Task 2.2: 草稿切换 + 聊天发送

**Files:**
- Modify: `src/tui/app.rs` (on_key 里的发送逻辑、草稿切换)
- Modify: `src/tui/manager_conn.rs` (验证 send_prompt)

- [ ] **Step 1: Ctrl+Enter 发送消息**

```rust
// app.rs on_key:
KeyCode::Enter => {
    if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        // 保存草稿后发送
        if let Some(sid) = &self.active_session {
            if !self.input_text.is_empty() {
                let text = self.input_text.clone();
                self.drafts.insert(sid.clone(), text.clone());
                let _ = self.conn.send_prompt(sid, &text).await; // on_key 不能 async — 需改造
                self.input_text.clear();
                self.input_cursor = 0;
            }
        }
    }
}
```

注意：`on_key` 目前是同步函数。发送消息需要异步。两种方案：
1. 在 `on_key` 里用 `tokio::spawn` 发送（fire-and-forget）
2. 把输入队列化，tick 时处理

推荐方案 1：

```rust
// 在 AppState 加 pending_sends: Vec<String>
if key.modifiers.contains(KeyModifiers::CONTROL) {
    if let Some(sid) = &self.active_session {
        let text = self.input_text.clone();
        self.drafts.insert(sid.clone(), text.clone());
        self.pending_sends.push((sid.clone(), text));
        self.input_text.clear();
        self.input_cursor = 0;
    }
}

// on_tick 里处理 pending_sends:
for (sid, text) in self.pending_sends.drain(..) {
    let _ = self.conn.send_prompt(&sid, &text).await;
}
```

- [ ] **Step 2: 切换选中时保存/恢复草稿**

```rust
// navigate_up/down 或 on_enter 时：
fn switch_session(&mut self, new_sid: &str) {
    // 保存当前草稿
    if let Some(old_sid) = &self.active_session {
        self.drafts.insert(old_sid.clone(), self.input_text.clone());
    }
    // 加载新草稿
    self.active_session = Some(new_sid.to_string());
    self.input_text = self.drafts.get(new_sid).cloned().unwrap_or_default();
    self.input_cursor = self.input_text.len();
}
```

- [ ] **Step 3: 编译 + commit**

```bash
cargo build && git add src/tui/app.rs && git commit -m "feat(tui): chat send and draft persistence"
```

---

### Task 2.3: 动画 + 精致打磨

**Files:**
- Modify: `src/tui/app.rs` (tick 里的动画帧)
- Modify: `src/tui/view/kanban.rs` (呼吸动画)

- [ ] **Step 1: 动画帧状态**

```rust
// AppState 加：
pub anim_frame: u8,  // 0..=7，每 tick 递增
```

```rust
// on_tick:
self.anim_frame = (self.anim_frame + 1) % 8;
```

- [ ] **Step 2: 呼吸特效（Busy 状态闪烁）**

在 kanban.rs 的 status_icon 使用时：

```rust
let alpha = if status == "busy" {
    // 用 anim_frame 控制透明度
    if state.anim_frame < 4 { 1.0 } else { 0.4 }
} else { 1.0 };
// ratatui 不支持透明度，改方法：交替显示/隐藏 ▸ 符号
let icon = if status == "busy" && state.anim_frame >= 4 {
    " " // 隐藏图标模拟闪烁
} else { icon_from_status };
```

- [ ] **Step 3: 响应式调整 — 自动重算**

在 `run_inner` 的渲染循环里已自动重算 layout_tier（`on_key` 的 `Resize` 事件）。确保宽度变化时重新计算 `cards_per_row`。

- [ ] **Step 4: 编译 + commit**

```bash
cargo build && git add src/tui/ && git commit -m "feat(tui): add animations and responsive polish"
```

---

### Task 2.4: 错误处理 + 断线重连 + 空状态

**Files:**
- Modify: `src/tui/manager_conn.rs` (重连逻辑已在 request 里)
- Modify: `src/tui/app.rs` (on_tick 错误处理)
- Modify: `src/tui/view/*.rs` (空状态展示)

- [ ] **Step 1: 空状态展示**

确保每个 view 在 state.workers 为空时显示正确信息：
- tree：显示项目数 0
- kanban：显示 "No active workers. Start one with `ion rpc --method create_session`"
- detail：显示 "Select a card to view details"

- [ ] **Step 2: 断线状态**

在状态栏显示 `○ disconnected`，app 自动每 2 秒尝试重连（在 `on_tick` 里加 retry 逻辑）。

- [ ] **Step 3: 退出确认**

按 q 时显示退出提示（但 MVP 直接退出即可）。

- [ ] **Step 4: 最终编译 + commit**

```bash
cargo build --bin ion && git add . && git commit -m "feat(tui): error handling and polish"
```
