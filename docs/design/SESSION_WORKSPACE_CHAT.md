# 会话内创建独立工作空间聊天

> **状态：开发中** — 需求和实现边界已明确，当前尚未实现 UI 入口和统一 RPC 闭环。

## 概览

本功能模拟截图中的交互：用户在当前对话中提出一个适合独立处理的任务，系统在当前对话内显示一张“已安排独立工作空间”的结果卡片。系统同时创建一个新的 Session，并为它绑定独立的 Git worktree。用户点击卡片或左侧会话列表后，UI 跳转到这个新 Session 的聊天页面。

这里的“跳转”是 UI 层的 active-session 切换，不是把现有 RPC client 重新绑定到另一个 Session。新页面必须携带并使用 `sessionId + projectPath + sessionPath`，重新加载该 Session 的状态和事件订阅。

| 能力 | 入口 | 当前状态 |
|------|------|----------|
| 从当前对话创建新的 Session | `create_session` / 新增 workspace 参数 | 🔧 需要补齐 |
| 为新 Session 创建独立 Git worktree | `create_worker.worktree` 已有 | ✅ 内核已有，需接入统一流程 |
| 记录父子 Session 关系 | `parent_session` / `parent_type` | ✅ 已有基础能力 |
| 当前对话显示工作空间结果卡片 | UI 组件 | 🔧 待开发 |
| 点击卡片跳转到新 Session | UI active-session 路由 | 🔧 待开发 |
| 新连接恢复当前 Session 状态 | `get_state` / `get_messages` / `subscribe` | ✅ 能力已有，需组合 |
| 多窗口同步新 Session 创建 | EventBus / subscribe | 🔧 需要统一事件类型 |
| 纯 HTML 可点击原型 | `ion-orbit-ui/pages/session-workspace-demo.html` | ✅ 已实现（Mock 模式，jsdom 45 断言验收通过） |

## 需求范围

### 必须支持

1. 用户在 Session A 中发起“为这个任务安排独立工作空间”的请求。
2. 系统创建 Session B。
3. Session B 具备以下元数据：
   - `sessionId`
   - `parentSessionId`
   - `projectPath`
   - `workspacePath`
   - `branch`
   - `baseRef`
   - `status`
   - `route`
4. Session B 的 Worker 在 `workspacePath` 中启动。
5. Session A 收到一个可渲染的 `workspace_session_created` 事件或消息卡片。
6. 用户点击“打开工作树聊天”后，UI 切换到 Session B。
7. 切换后必须先拉取 Session B 的完整快照，再接收实时事件。
8. 刷新页面或重新连接后仍能恢复 Session B，而不能只依赖一次性前端事件。
9. Session A 和 Session B 的聊天历史互相隔离。
10. 关闭 Session B 时，Worktree 目录按清理策略删除，Git 分支默认保留。

### 不在本期范围内

- 不实现真实音视频或网络媒体传输。
- 不把 Session B 的聊天内容复制回 Session A。
- 不通过浏览器本地状态模拟真实持久化。
- 不使用 RPC client-level 的 `switch_session` 重新绑定已有连接。
- 不要求用户直接看到 Worker 内部 ID；UI 以 Session ID 和 workspace 元数据为主。

## 现有代码核查结果

### 已有能力

- Session Tree 已支持从分支点 fork 新 Session：
  `ion --fork-from-leaf <sid>/<entry-id>`。
- `WorkerCreateConfig` 已支持：
  `worktree`、`project_path`、`session`、`parent`、`relation`、`initial_prompt`。
- Worktree 创建已经支持 `branch` 和 `base`。
- Worker 进程会以 Worktree 目录作为 `current_dir` 启动。
- Worker 事件可以通过 `subscribe --session <sid>` 实时推送，并支持最近事件回放。
- `list_all_sessions` 已有 `parentSession`、`parentType` 等血缘字段。

### 当前缺口

1. `create_session` 当前只读取 `agent`、`session_id`、`model`、`provider`、`project_path/cwd`、`initial_prompt`，没有把 Worktree 配置接入统一创建流程。
2. `create_worker` 返回 `workerId + sessionId`，但不适合作为 UI 的业务级“创建工作空间会话”接口。
3. 当前没有统一的 `WorkspaceSession` 响应对象，UI 需要从多个接口拼接 workspace 信息。
4. 当前没有专门的 `workspace_session_created` / `workspace_session_resolved` 事件协议。
5. 当前 UI 页面是静态展示，尚未有 active-session 路由和会话切换服务。
6. 当前没有一个原子操作保证“创建 Worktree、创建 Session、启动 Worker、发送创建事件”全部成功或回滚。

### 已修复（2026-08-18，P0 批次）

- **project_path 透传**：`spawn_worker` bridge（`src/runtime.rs`）现在把当前 Worker 的 cwd 作为 `project_path` 发给 Manager，不再回退 host cwd。修复前实测：子 worker 项目落在 host 工作目录，非 git 目录还会被自动 `git init` + 全量 commit；修复后子 worktree 正确从父会话项目切出。
- **三层响应补 worktree 元数据**：Manager `create_worker` 响应（含 child+wait 延迟响应路径）、bridge `SpawnWorkerResponse`、`spawn_worker` 工具返回 JSON 三层都带 `worktree_path` / `worktree_branch`（未隔离时为 null）。
- 验证：单元测试 `runtime::tests::spawn_worker_bridge_*` + RPC 实测（call_tool 直调 spawn_worker，响应含 worktree 元数据、子会话 project 正确）。
- 实测结论（同日）：真实 glm-5.2 在零提示词引导下即可据工具 schema 触发 `spawn_worker(worktree:true)`；faux 脚本为 per-process 重放，spawn 链 harness 必须用静态回复 + `call_tool`，否则递归 fork bomb。

### 已实现（2026-08-18，P1/P2 批次）——内核统一 RPC 闭环 ✅

对应下方 §3 的 RPC 接口规格，全部已落地并通过 `tests/session_workspace_ci.sh`（26/26）：

| 能力 | 实现 | 验证 |
|------|------|------|
| `WorkspaceSession` 数据模型 + 持久化 | `src/session_workspace.rs`（`~/.ion/agent/workspaces.json`，原子写，重启可恢复） | 单测 3 个 + CI Group F |
| `create_workspace_session` 原子 RPC | `src/bin/ion.rs`（worktree+session+worker 一次完成；失败回滚半成品 worktree；`require_clean` 脏源拒绝） | CI Group A/D |
| `workspace_session_*` 事件 | creating/created/ready/failed/closed 五类；双路投递：实例事件流（`subscribe --session 父`，含 replay buffer，走 `push_session_event`）+ EventBus（route=ui，webui 可达） | CI Group C/E |
| `close_workspace_session` RPC | 默认删 worktree 目录、保留分支；`cleanup_worktree=false` 保留目录；`delete_branch=true` 可删分支 | CI Group E |
| `get_session_snapshot` RPC | workspace 元数据（运行态合并 idle/running）+ worker 状态 + 最近 20 条消息（直读 JSONL，worker 不在也能恢复） | CI Group B/E |
| spawn_worker 工具暴露 branch/base | schema 新增可选 `branch`/`base`，LLM 可命名工作区分支（缺省仍自动 `ion-worker-*`） | bridge 单测 |

实现说明：
- 事件外壳对齐 BashExtension 格式（`{"type":"event","event":{"type":"extension_event","extension":"workspace","customType":...}}`）
- session index 的 `parentType` 记为 `child`（worker 树语义）；workspace 层的 `parentType=workspace` 由 WorkspaceSession 元数据承载
- 快照消息解析：子会话 `<sid>.jsonl` 优先、主会话共享 `session.jsonl` 兜底
- lib 测试 982 全过（含新增 store 3 个 + bridge 2 个）；CI 脚本完全隔离（HOME/SESSION_DIR/WORKTREE_ROOT/SOCKET 指向 `/tmp/ion-session-workspace-test.XXXXXX`）

### 已实现（2026-08-18，推送完备性 + LLM 路径统一）✅

- **LLM 路径统一管线**：`spawn_worker(worktree:true)`（LLM 一句话触发）与显式 `create_workspace_session` 同构——自动持久化 WorkspaceSession + 广播 `workspace_session_created`，卡片/侧栏对两种触发方式行为一致。工具响应三层新增 `session_id`（UI 可订阅子会话）。
- **任何会话产生/终止必推送**：`register_prepared_worker` 广播 `session_created`、`kill_worker` 广播 `session_closed`（EventBus route=ui，带 sessionId/workerId/project/parentSession）——接收方接不接收是它的事，发送方一定推。原有 `worker_created`/`worker_destroyed` global 通道（host stdout）保留不变。
- CI 新增 Group G（LLM spawn 路径：事件 + 持久化 + 响应元数据）与 Group H（subscribe --ui 验证产生/终止推送必达）。

### 触发语义（最终定论，2026-08-19）✅

**工作空间的创建由 LLM 判断，不做页面直通**。消息发给会话 A 的 LLM → 它评估任务是否适合隔离 → 适合则调 `spawn_worker(worktree:true)` → 内核创建 → 事件推送 → 卡片才出现。曾短暂实现过"首条消息页面直建"（确定性高但跳过判断），评审后废弃——**智能判断是产品语义的核心，卡片出现的时机必须晚于 LLM 的决策**（实测判断耗时约 7~40s，这个等待是特性不是延迟）。判断依据来自用户措辞的自然语言（"比较独立/别影响主分支"等）。

### 已实现（2026-08-18 晚，架构收敛评审后）✅ —— 门面 RPC 删除

评审结论：**子进程机制（create_worker/spawn_worker）本身已具备全部能力，业务级门面属于多余层级**。收敛内容：

- **删除** `create_workspace_session` / `close_workspace_session` 两个门面 RPC（保留 `get_session_snapshot`——远端 UI 读不了本地文件，服务端组合有真实增量）
- **下沉到基础设施**：`create_worker` 新增 `require_clean` 参数（脏源拒绝）+ spawn 失败自动回滚半成品 worktree；`kill` 新增 `cleanupWorktree`/`deleteBranch` 参数（默认删目录留分支），并修复了 kill RPC 从顶层读参数的存量 bug
- **事件/持久化统一挂到 register/kill**：任何创建方式（create_session / create_worker RPC / LLM spawn_worker）都发 `workspace_session_created` + `session_created`；任何销毁发 `workspace_session_closed` + `session_closed`——不依赖门面
- **UI 接线对齐评审语义**：卡片样式绑定 spawn 方法（事件驱动）、点击超链接切会话（前端路由）、会话列表由推送自动增删（网关新增 `/ui-events` SSE 通道转发 EventBus ui 流）
- 验证：CI **38/38**（A/E/D 组改走 create_worker/kill）、live 端到端 **15/15**（双 SSE 通道 + 清理策略）、mock 回归 20/20、lib 982 全过

### Bug 修复（2026-08-18 深夜）：worktree "复用"路径返回死路径

- **现象**：真实 LLM 场景 `spawn_worker(worktree:true, branch=已存在分支)` 静默失败——子进程 spawn 报 `No such file or directory`，无事件无卡片（faux/新分支场景不复现，故 CI 未覆盖）
- **根因**：`create_worktree_advanced` 在 `git worktree add -b` 报 "already exists" 时，"复用"分支直接返回**从未被 git 填充的新构造路径**（只有 mkdir 的父目录），子进程 `current_dir` 指向不存在的目录 → ENOENT
- **修复**：复用时经 `git worktree list --porcelain` 解析该分支**真实存在**的 worktree 路径；无可复用则 prune 后以 "checkout 已有分支"语义（不带 -b）重试；仍失败返回明确错误。附带 spawn 失败回滚留下的空目录已清理
- **定位手段**：网关层复刻用户全流程（真实 glm-5.2 经 prompt→spawn），SSE 捕获 tool_execution_end 的错误信息直接暴露根因——这是"命令行可验证原则"的直接收益

### Bug 修复（同夜续）：push_session_event 只推首个匹配

- **现象**：spawn 成功、workspaces.json 记录正确（parentSessionId 正确），但 `workspace_session_created` 不进父会话事件流（subscribe 回放缓冲里查无此事件）——卡片/侧栏无动静
- **根因**：同一 session 可能存在多条 worker 记录（prompt 自动复活等场景产生新旧两条），`push_session_event` 用 `find()` 只推第一个匹配——事件可能投进没有订阅者的旧缓冲
- **修复**：推给**所有** session 匹配的 worker 记录；A/B 对照（child+wait / peer+nowait）与网关→host 端到端均验证事件到达

### 已实现（2026-08-18，UI live 接线）✅

- **`ion-orbit-ui/workspace_gateway.py`**：浏览器 ⇄ HTTP/SSE ⇄ host Unix socket 的本地网关（纯 Python 标准库）。`POST /rpc`（含 session 级路由）/ `GET /events?session=`（SSE，断线自动重连，遵守"每连接一条命令"与 600s 空闲约束）/ 静态供给 `/pages/*`。
- **demo 页 live 模式**：URL 加 `?live=1` 启用，同一套状态机由真实 RPC+SSE 驱动——`create_session` 建主会话 → 卡片按钮触发真实 `create_workspace_session` → SSE 事件驱动 FSM → 打开 B 先 `get_session_snapshot` Pull 再订阅 → 发消息走真实 `prompt`（`text_delta` 流式渲染）→ 关闭走 `close_workspace_session`。默认无参数仍是纯 Mock 模式。
- 自测：mock 回归 jsdom 45/45 不破；live 链路 curl 端到端 14/14（隔离 host + faux：静态页/RPC 透传/SSE 四类事件/session 级 prompt/快照/关闭清理）。
- 用法：`ion serve` → `python3 ion-orbit-ui/workspace_gateway.py` → 打开 `http://127.0.0.1:8789/pages/session-workspace-demo.html?live=1&project=<git仓库路径>`

## 1. 推荐架构

```text
当前 Session A
      │
      │ create workspace session
      ▼
Host / SessionWorkspaceService
      │
      ├── 创建 Git worktree
      ├── 创建 Session B 元数据
      ├── 启动 Worker B
      ├── 持久化 parentSession / workspace 信息
      └── 广播 workspace_session_created
                │
                ▼
        UI 结果卡片 / Session 列表
                │ click
                ▼
      activeSession = Session B
      │
      ├── get_session_snapshot
      └── subscribe(session=B)
```

### 内核还是 UI

这是内核能力 + UI 消费的功能：

- Session、Worktree、Worker 生命周期和事件属于内核。
- 卡片展示、点击跳转、当前 Session 选择属于 UI/service 层。
- HTML 原型只模拟 UI，不直接修改真实 Session 文件。

### Session 切换边界

UI 不应该把已有连接从 Session A 改绑到 Session B。推荐流程是：

1. 从列表中选择 `sessionId + projectPath + sessionPath`。
2. 关闭或保留当前 Session A 的订阅，按产品策略决定。
3. 获取 Session B 的恢复快照。
4. 创建或复用 Session B 自己的 RPC 服务连接。
5. 订阅 Session B 的实时事件。

这样可以避免一个 Session 的实时连接被另一个 Session 占用。

## 2. 数据结构

### 2.1 WorkspaceSession

```json
{
  "sessionId": "sess_widget_rt",
  "parentSessionId": "sess_main_001",
  "parentType": "workspace",
  "projectPath": "/Users/user/Project/ion",
  "sessionPath": "/Users/user/.ion/agent/sessions/.../sess_widget_rt.jsonl",
  "workspacePath": "/Users/user/.ion/worktrees/91ab2c3d/ion",
  "branch": "feat/widget-layout-editor",
  "baseRef": "feat/widget-layout-editor",
  "title": "实时路径收口",
  "status": "running",
  "route": "#/sessions/sess_widget_rt",
  "createdAt": 1787000000000
}
```

### 2.2 状态枚举

```text
creating → ready → running → idle → closed
       └→ failed
```

UI 在 `creating` 时显示加载状态，在 `ready/running` 时显示可点击入口，在 `failed` 时显示错误和重试按钮。

### 2.3 创建事件

```json
{
  "type": "extension_event",
  "customType": "workspace_session_created",
  "session": "sess_main_001",
  "data": {
    "workspaceSession": {
      "sessionId": "sess_widget_rt",
      "parentSessionId": "sess_main_001",
      "workspacePath": "/Users/user/.ion/worktrees/91ab2c3d/ion",
      "branch": "feat/widget-layout-editor",
      "status": "ready",
      "route": "#/sessions/sess_widget_rt"
    }
  }
}
```

事件只负责通知；刷新和重连时必须通过 Pull RPC 查询当前状态。

## 3. RPC 接口规格

### 3.1 `create_workspace_session`

建议新增 Manager 级 RPC。它封装 Worktree、Session、Worker 的创建，不让 UI 自己串接多个底层 RPC。

**请求：**

```bash
ion rpc --method create_workspace_session \
  --params '{
    "parent_session_id":"sess_main_001",
    "agent":"developer",
    "project_path":"/Users/user/Project/ion",
    "workspace":{
      "enabled":true,
      "branch":"feat/widget-layout-editor",
      "base":"feat/widget-layout-editor",
      "require_clean":true
    },
    "title":"实时路径收口",
    "initial_prompt":"在独立工作空间中处理实时路径收口任务"
  }'
```

**成功响应：**

```json
{
  "success": true,
  "data": {
    "workspaceSession": {
      "sessionId": "sess_widget_rt",
      "parentSessionId": "sess_main_001",
      "workspacePath": "/Users/user/.ion/worktrees/91ab2c3d/ion",
      "branch": "feat/widget-layout-editor",
      "status": "ready",
      "route": "#/sessions/sess_widget_rt"
    }
  }
}
```

**失败响应：**

```json
{
  "success": false,
  "error": "source branch has uncommitted changes"
}
```

### 3.2 `get_session_snapshot`

用于页面首次打开、刷新和重连恢复。

```bash
ion rpc --session sess_widget_rt --method get_session_snapshot
```

**响应内容必须包含：**

- Session 元数据；
- parent Session；
- workspace 路径、分支和状态；
- 最近消息；
- 当前 Worker 状态；
- 最近未完成的任务或工具调用；
- 当前事件回放游标。

如果短期内不新增此 RPC，可以先组合：

```text
list_all_sessions + get_state + get_messages + subscribe(replay=N)
```

但正式实现建议提供统一快照接口，避免 UI 自己拼接不完整状态。

### 3.3 `subscribe`

继续复用现有实时订阅：

```bash
ion subscribe --session sess_widget_rt --replay 100
```

需要新增或统一以下事件：

```text
workspace_session_creating
workspace_session_created
workspace_session_ready
workspace_session_failed
workspace_session_closed
```

### 3.4 `close_workspace_session`

不要让 UI 直接猜测 `kill` 是否会删除 Worktree。建议增加显式关闭接口：

```bash
ion rpc --method close_workspace_session \
  --params '{
    "session_id":"sess_widget_rt",
    "cleanup_worktree":true,
    "delete_branch":false
  }'
```

默认策略：删除 Worktree 目录，保留 Git 分支。

### 3.5 UI 跳转不是 RPC

点击卡片只需要更新 UI 路由：

```text
当前地址：#/sessions/sess_main_001
点击卡片：#/sessions/sess_widget_rt
```

跳转后再调用 `get_session_snapshot` 和 `subscribe`。不要增加一个“让 Host 切换当前 UI 会话”的 RPC。

## 4. HTML 原型范围

### 文件位置

```text
ion-orbit-ui/pages/session-workspace-demo.html
```

这是仓库内的可审查原型，不放 `/tmp`，原因是它属于产品交互设计资产，需要能够提交、评审和重复打开。

### 原型不连接真实 Host

第一阶段只使用内置 Mock 数据：

```js
const mockSessions = {
  main: { id: "sess_main_001", ... },
  workspace: { id: "sess_widget_rt", ... }
};
```

原型必须模拟：

1. 当前对话中的结果卡片；
2. 卡片点击创建/打开 Session B；
3. 左侧 Session 列表出现 Session B；
4. 点击 Session B 后显示独立聊天页面；
5. 返回 Session A；
6. URL hash 路由变化；
7. `creating`、`ready`、`running`、`failed` 四种状态；
8. 没有真实 RPC 时显示“模拟模式”。

### 原型验证方式

```bash
python3 -m http.server 8787 --directory ion-orbit-ui
open http://127.0.0.1:8787/pages/session-workspace-demo.html
```

原型测试不创建真实 Session、不写 `~/.ion`、不创建 Git branch。

## 5. 持久化目录与测试目录

### 正式运行目录

正式运行沿用 ION 现有目录约定：

```text
~/.ion/agent/sessions/        # Session JSONL 和索引
~/.ion/worktrees/              # Git worktree 工作目录
```

Worktree 只存工作文件；Session 元数据和消息不放进 Git worktree，避免工作目录删除后丢失会话历史。

### 原型目录

```text
ion-orbit-ui/pages/session-workspace-demo.html
```

原型使用 Mock 数据，不产生正式运行数据。

### 自动化测试目录

测试必须使用临时目录，不污染真实工程：

```text
/tmp/ion-session-workspace-test.XXXXXX/
├── repo/                    # 测试 Git 仓库
├── worktrees/               # ION_WORKTREE_ROOT 指向这里
├── host.sock                # ION_HOST_SOCKET 指向这里
└── artifacts/               # RPC 输出、事件流、失败日志
```

测试脚本启动 Host 后只记录并终止自己的 `HOST_PID`，禁止使用宽泛的 `pkill -f`。

如果后续需要完全隔离 Session 持久化目录，应在实现前增加显式的 ION 数据根目录配置，例如 `ION_DATA_ROOT`，让测试可以把 Session 数据也重定向到临时目录；在此之前不把真实 `~/.ion/agent/sessions/` 当作测试目录。

## 6. 开发前自检门禁

开始写代码前，必须逐项确认：

| # | 自检项 | 通过标准 |
|---|--------|----------|
| 1 | 用户动作 | 明确是“当前 Session 创建子 Session 并点击跳转”，不是媒体传输功能 |
| 2 | Session 关系 | 明确 `parentSessionId` 和 `parentType=workspace` 的含义 |
| 3 | 工作空间 | 明确 Worktree 从哪个 `baseRef` 创建，是否要求源目录干净 |
| 4 | 创建原子性 | Worktree、Session、Worker 任一失败时有回滚或明确 failed 状态 |
| 5 | UI 跳转 | 明确使用 `sessionId + projectPath + sessionPath`，不重绑已有 RPC client |
| 6 | 恢复机制 | 页面刷新先 Pull 快照，再接收 Push 事件 |
| 7 | 事件协议 | 已定义创建、就绪、失败、关闭四类事件 |
| 8 | 清理语义 | 明确 Worktree 是否删除、分支是否保留 |
| 9 | 原型隔离 | HTML 原型不连接真实 Host，不写正式 Session 数据 |
| 10 | 测试隔离 | 自动化测试使用临时 repo、临时 Worktree root 和精确 Host PID |
| 11 | 可观察性 | 关键状态可通过 RPC 查询或 subscribe 观察 |
| 12 | 术语 | 全部使用 Session、Worktree、Extension，不引入已废弃术语 |

如果第 1、4、5、6、8、9、10 项任意一项没有答案，不进入实现阶段。

## 7. 实现任务清单

### Task 1：补齐 WorkspaceSession 数据模型

**文件：**

- 修改：`src/session_index.rs`
- 修改：`src/worker_registry.rs`
- 可能新增：`src/session_workspace.rs`
- 测试：`tests/session_workspace_harness.rs`

**内容：**

- 增加 workspace 元数据；
- 记录 parent Session；
- 记录 workspace path、branch、base ref、状态；
- 统一序列化和恢复。

### Task 2：实现统一创建 RPC

**文件：**

- 修改：`src/bin/ion.rs`
- 复用：`src/worker_registry.rs`
- 测试：`tests/session_workspace_ci.sh`

**内容：**

- 新增 `create_workspace_session`，或扩展 `create_session` 的 `workspace` 参数；
- 创建前检查源分支是否 clean；
- 创建 Worktree；
- 创建独立 Session 文件；
- 启动 Worker；
- 广播创建事件；
- 失败时清理半成品。

### Task 3：实现 Pull/Push 状态闭环

**文件：**

- 修改：`src/bin/ion.rs`
- 修改：`src/event_bus.rs`
- 修改：`src/worker_registry.rs`
- 测试：`tests/session_workspace_ci.sh`

**内容：**

- `get_session_snapshot`；
- `workspace_session_*` 事件；
- `subscribe --replay` 恢复；
- 多窗口同步。

### Task 4：实现 UI Session 路由和结果卡片

**文件：**

- 新增：`ion-orbit-ui/pages/session-workspace-demo.html`
- 后续集成：现有会话页面或 Web UI Session service

**内容：**

- 结果卡片；
- 新 Session 侧栏项；
- hash 路由；
- active-session 切换；
- loading/failed/ready 状态；
- Pull 后订阅。

### Task 5：增加两层验证

**文件：**

- 新增：`tests/session_workspace_harness.rs`
- 新增：`tests/session_workspace_ci.sh`
- 可选：`tests/session_workspace_e2e.rs`，标记 `#[ignore]` 并要求 `ION_E2E=1`

**要求：**

- Harness 使用 FauxProvider，不调用真实 LLM；
- CI 脚本从命令行启动 Host、创建 Session、读取 RPC、监听事件；
- 真实 E2E 验证实际 LLM 创建子 Session 的场景；
- UI 原型至少完成点击跳转和 URL 状态检查。

## 8. 开发完成后的自检

### RPC/内核

- [ ] Session A 创建 Session B 成功；
- [ ] Session B 有独立 Session 文件；
- [ ] Session B 使用独立 Worktree；
- [ ] 源分支 dirty 时按配置拒绝创建；
- [ ] 创建失败不会留下半成品 Worktree；
- [ ] `get_session_snapshot` 可在刷新后恢复完整状态；
- [ ] `subscribe` 能收到创建、就绪、失败、关闭事件；
- [ ] Session B 关闭后目录清理、分支保留符合配置。

### UI

- [ ] 当前对话能显示结果卡片；
- [ ] 卡片显示标题、状态、分支和工作目录摘要；
- [ ] 点击卡片跳转到 Session B；
- [ ] 左侧列表同步出现 Session B；
- [ ] 从 Session B 返回 Session A 不丢失状态；
- [ ] 刷新 `#/sessions/<sid>` 后仍能恢复对应 Session；
- [ ] 创建失败显示重试入口；
- [ ] 原型模式不会触碰真实 Host 和真实 Session 数据。

### 命令行验证

```bash
cargo test --test session_workspace_harness
bash tests/session_workspace_ci.sh
```

真实场景：

```bash
ION_E2E=1 cargo test --test session_workspace_e2e -- --ignored
```

## 9. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | 评审 `create_workspace_session` 与扩展 `create_session.workspace` 的接口选择 | P0 |
| 2 | 实现 `WorkspaceSession` 元数据和快照接口 | P0 |
| 3 | 实现 HTML Mock 原型并完成点击跳转验收 | P0 |
| 4 | 实现 Host RPC、事件和 Worktree 原子创建 | P1 |
| 5 | 接入真实 Web UI Session service | P1 |
| 6 | 增加 `ION_DATA_ROOT` 测试隔离配置 | P1 |
