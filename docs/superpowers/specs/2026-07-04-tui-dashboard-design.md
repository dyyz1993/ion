# ION TUI Dashboard 设计文档

> **状态：已完成** — 已通过用户评审，待进入实施。

## 概述

为 ION 操作系统开发一个终端 TUI（Terminal UI）仪表板，运行方式为 `ion dashboard`。允许用户在一个面板中实时查看多个 Worker 的运行状态、执行情况，并与 Worker 进行交互式聊天。

## 内核改动（前置条件）

### 补丁 A：Worker 状态流转

`src/worker_registry.rs` 中 `WorkerRecord.status` 目前只有 `Idle`/`Dead`（`WorkerStatus` 枚举），需要补全流转：

| 触发点 | 变化 | 位置 |
|--------|------|------|
| `create_worker` 完成 | → `Idle` | 已有 |
| `send()` / `prompt` 入队后 | `Idle` → `Busy` | `send()` / `handle_manager_command.send` |
| `agent_end` 事件 | `Busy` → `Idle` | `drain_until_agent_end` 循环 |
| heartbeat 超时 > 60s | * → `Stale` | 新增后台 Task（Manager 主循环） |
| `kill` | * → `Dead` | 已有 |

新增枚举成员：`WorkerStatus::Stale`（序列化 `"stale"`）。

每次状态变更时调用 `WorkerRegistry::broadcast_overview()` 通知所有 overview 订阅者。

### 补丁 B：subscribe_overview

Manager socket（`src/bin/ion.rs`）新增命令 `subscribe_overview`。

- `WorkerRegistry` 内加 `overview_subs: Vec<mpsc::UnboundedSender<Value>>`。
- `broadcast_overview()` 在状态变化点调用（去抖 100ms，短时间内多次变化合并一次推送）。
- 订阅者收到 `{"type":"overview_snapshot","data":{/* 同 get_overview payload */}}`。
- 退订检测：rx drop 时自动清理。

### 补丁 C：WorkerRecord 补充字段

TUI 需要的信息：
- `latest_output: Vec<String>` — 最近几行关键输出（Agent 端流式写入，每次 text_delta 追加，保留最后 N 行）
- `uptime_secs` — 运行时（已有 `started_at`，计算 `now - started_at`）
- `model_size: Option<String>` — 模型上下文窗口人类可读表示（如 "128k"）

## 架构

### 形态

`ion dashboard` 子命令，复用 `src/bin/ion.rs` 已有的 socket 连接代码。

### 依赖

```toml
ratatui = "0.29"
crossterm = "0.28"
```

### 进程模型

单进程 tokio 事件循环，三路事件源：

1. **Manager socket**（`manager_conn.rs`） — `subscribe_overview` + `subscribe session` + `send`
2. **终端事件**（crossterm） — 键盘/鼠标
3. **Tick**（tokio interval 4Hz） — 动画时钟 / 时长更新

三路用 `tokio::select!` 合并到 AppState，驱动绘制。

```
                          ┌───────────────┐
                          │  tick 4Hz     │
                          │  (tokio task) │
                          └──────┬────────┘
                                 │ tick()
         ┌──────────────┐       ▼
         │ ManagerConn  │────► AppState ────► ratatui::Terminal::draw()
         │ (Unix sock)  │       │
         └──────────────┘       │◄──── terminal events
                                │        (crossterm)
                          events / send()
                                │
                                ▼
                         Manager Socket
```

### 数据流

```
manager_conn
  ├── subscribe_overview → AppState.overview (workers/projects 快照)
  │      └── diff → update kanban cards, tree nodes
  ├── subscribe {session} → AppState.log_buffers[sid] (实时输出)
  │      └── append to scrollback buffer per session
  └── send {method:"send", session, "input":...} → 聊天发送
```

## 布局（响应式）

### Wide（≥ 140 列）三栏

```
┌─ Tree ──┐ ┌─ Kanban (卡片网格) ──────────────────┐ ┌─ Detail (选中) ──┐
│ 项目树   │ │ 卡片网格，每行最多 N=floor(可用宽/50) │ │ 详细信息/聊天   │
│ 可折叠   │ │ 每张卡片：状态色条 + icon + 信息      │ │ 空则显示提示    │
│ 过滤看板 │ └──────────────────────────────────────┘ │                  │
└─────────┘                                          └──────────────────┘
```

### Medium（80–139 列）两栏

树 | 看板（详情按 `d` / 回车浮层弹出）

### Narrow（< 80 列）单栏栈式

纯看板，纵向排列

### Focus 模式（双击卡片）

详情占据 >= 70% 宽度，右侧展开 Todo + Memory 侧栏（各占约 15%）。

## 主题（赛博朋克）

### 调色板

| 用途 | Hex |
|------|-----|
| 背景 | `#0a0e1a` |
| 面板 | `#111827` |
| 主文字 | `#c8d3f5` |
| 灰文字 | `#5b6b9c` |
| 强调（青） | `#00ffd1` |
| 危险/忙（品红） | `#ff2d95` |
| 警告/Stale | `#ffb800` |
| 死亡/错误 | `#7a1f3d` |
| 边框聚焦 | `#00ffd1` |
| 边框普通 | `#1f4d5c` |
| 边框失焦 | `#2a3349` |

### 细节

- 面板聚焦时边框青亮，失焦灰
- 卡片状态：`▶` 品红呼吸（Busy）、`⏸` 青（Idle）、`⚠` 琥珀（Stale）、`⨯` 暗红（Dead）
- 等宽字体，bold 高亮关键数据
- 新卡片出现淡入动画（首 300ms）

## 状态管理（AppState）

```rust
struct AppState {
    overview: OverviewSnapshot,           // 来自 subscribe_overview
    selected: Option<NodeId>,             // 当前选中（树节点或卡片）
    focus_mode: bool,                     // 双击放大
    focused_panel: Panel,                 // 当前焦点所在面板
    tiers: LayoutTier,                    // Wide / Medium / Narrow
    collapsed: HashSet<String>,           // 折叠的项目名
    tree_filter: String,                  // 搜索过滤字符串
    log_buffers: HashMap<Sid, VecDeque<LogLine>>,    // 实时输出缓冲
    drafts: HashMap<Sid, String>,         // 输入框草稿
    config: TuiConfig,                    // 用户配置
}
```

## 交互

| 键 | 作用 |
|----|------|
| tab / shift+tab | 切换面板焦点 |
| hjkl / arrows | 导航 |
| enter | 选中/确认 |
| d / 双击 | Focus 模式 |
| esc | 退出 |
| z | 折叠项目 |
| ctrl+enter | 发送聊天 |
| / | 搜索过滤 |
| r | 强制刷新 |
| q | 退出 |

## 文件结构

```
src/
├── tui/
│   ├── mod.rs              # pub fn run_dashboard() 入口
│   ├── app.rs              # AppState + 事件循环 (select! 三路)
│   ├── manager_conn.rs     # Unix socket 客户端
│   ├── theme.rs            # 赛博朋克调色板
│   ├── layout.rs           # 响应式分栏
│   └── view/
│       ├── mod.rs          # render 分发
│       ├── tree.rs         # 左栏项目树
│       ├── kanban.rs       # 卡片网格
│       ├── detail.rs       # 详情/聊天
│       ├── todo_panel.rs   # Todo 插件面板
│       └── mem_panel.rs    # Memory 插件面板
├── bin/
│   └── ion.rs              # + "dashboard" 子命令
```

内核改动文件：
- `src/worker_registry.rs` — status 逻辑 + overview 广播
- `src/bin/ion.rs` — subscribe_overview 命令分发
