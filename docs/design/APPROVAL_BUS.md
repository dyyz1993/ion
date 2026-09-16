# 统一审批总线（Approval Bus）

> **状态：已验证** — M1+M2+M3 三分支语义统一完成：单一数据源
> （ApprovalBus，双登记路径幂等去重）+ 单一路由（`approval_respond` /
> `WorkerRegistry::execute_unified_approval`）+ 审批泵接线（by=pump）。
> `tests/approval_bus_ci.sh` 三合一全绿；schema 动态校验
> （`tests/rpc_schema_subscribe_test.rs::approval_bus_rpc_frames_match_schemas`）。

---

## 概览

ION 原有三套互不相通的审批家族，id / 条目形状 / 审批动词各不相同，外部桥接方
（webui / IM 机器人 / 手机推送）无法用一个协议接完：

| # | 家族 | 表 | 应答 RPC | 事件 |
|---|------|-----|---------|------|
| ① | UI Ask（worker 进程内 Ask / host 级 Ask） | `runtime::pending_ui()`（worker 或 host 进程内） | `ui_respond`（host 级，同源绑定）/ `ask_respond`（worker 命令） | `Ask` / `AskResolved` / `AskTimedOut` |
| ② | worker 级 file-snapshot 审批 | worker 内 `ApprovalManager`（session JSONL 持久） | `review_approve` / `review_reject`（session 级） | `ApprovalRequest` / `ApprovalResolved` / `ApprovalReset` |
| ③ | host 级远程动词审批 | `worker_registry::verb_approvals_global()` | `verb_review` | `verb_approval` |

统一审批总线在 **host 进程** 维护一张统一审批表（`ApprovalBus::global()`）：
三类来源注册进同一张表，条目形状统一（`ApprovalEntry`），id 统一 `apr_<8hex>`
前缀。出口全部长在既有协议上（host socket RPC / EventBus / subscribe
snapshot），**旧 API 一律不动**。

| 能力 | 入口 | 状态 |
|------|------|------|
| Pull（全量统一表） | RPC `approvals_pending` | ✅ |
| Respond（路由回来源执行） | RPC `approval_respond`（三类全闭环） | ✅ |
| Push（注册/移除/决定事件） | EventBus `ApprovalRequest` / `ApprovalResolved` / `ApprovalRemoved` | ✅ |
| snapshot 水合 | `subscribe(session)` 快照帧 `data.approvals` | ✅（向后兼容：`pendingApprovals` 保留） |
| 审批泵自动放行（policy map） | `sandbox_policy` per-kind + pump | ✅（file_snapshot/ui_ask/remote_verb；`by=pump`） |

---

## 1. 单一数据源：ApprovalBus 与双登记路径

**ApprovalBus 是唯一表**（内存态不落盘）。三条登记路径全部收敛于此，靠
**dedupe key 对齐**幂等去重（先到者胜出，后到 dedupe 命中不产生第二条）：

| 路径 | 触发 | 条目信息 | 归属 |
|------|------|---------|------|
| **pump 登记**（主路径） | serve pump 收 worker stdout 审批事件 → `try_register_from_worker_event` → `BusBackedSink` | **带 worker_id + session_id**（路由回 worker 必需） | worker Ask / file-snapshot |
| **host 直登** | `verb_gate` ask_on_deny → `sink().register(verb_entry(...))` → `BusBackedSink` | 带 worker_id + session_id | remote_verb |
| **EventBus 镜像**（兜底） | `wire_approval_bus` 监听 `subscribe_ui` → `mirror_ui_event_to_bus` | 无 worker（host 级 Ask / verb 事件的原始形状） | host 级 Ask / verb 兜底 |

**顺序保证**：serve pump 对每条 worker 事件先 `try_register`（同步）再
broadcast 到 EventBus；镜像监听是异步任务 → **pump 登记必先于镜像**，带
worker 信息的条目胜出。镜像随后 dedupe 命中（同 key），不覆盖。

### dedupe key / TTL 约定（两路径共用，`BusBackedSink` 与镜像同算法）

| kind | dedupe key | TTL | native id（payload.nativeRequestId） |
|------|-----------|-----|--------------------------------------|
| `ui_ask` | `ask:<request_id>` | 120s（对齐 Ask 等待窗） | `req_<hex>` |
| `remote_verb` | `verb:<requestId>` | 300s（对齐 verb 等待窗） | `vapp_<hex>` |
| `file_snapshot` | `fs:<path:status 签名>` | 无（持续型） | `appr_<ts>`（file-approval 原生 requestId） |

file_snapshot 采用**总线规范语义**（同文件集重发幂等；不做 worker 级快照
替换）——`resolve_kind_for_worker` 收到 "superseded" 是 no-op，dedupe 签名
兜底；per-path resolved / reset 负责收口。

### ApprovalSink（来源侧窄接口，`src/approval_sink.rs`）

- `BusBackedSink`：生产实现（serve 启动 `set_sink` 安装），转发 ApprovalBus
- `RecordingSink`：测试替身（lib 单测用，保留）
- `NoopSink`：未安装兜底（零行为）
- `try_register_from_worker_event`：「哪些事件算审批来源」的知识收敛点
  （Ask/AskResolved/AskTimedOut/ApprovalRequest/ApprovalResolved；其余忽略）
- `respond_route(entry, decision)`：路由决策纯函数（见 §3）

### 生命周期同步（镜像 + sink 双路，均幂等）

| 来源侧事件 | 总线动作 |
|-----------|---------|
| `Ask` | 注册 ui_ask（带 TTL） |
| `AskResolved` | mark_resolved（allow→approve / 其它 reject） |
| `AskTimedOut` | mark_resolved（reject + by=system） |
| `verb_approval` / verb_gate ask_on_deny | 注册 remote_verb（带 TTL） |
| verb 超时 | resolve（reject + by=system） |
| `ApprovalRequest`（file-approval） | 注册 file_snapshot（同集幂等） |
| `ApprovalResolved`（per-path） | 收口 payload.files 含该 path 的条目（精收） |
| `ApprovalReset` | 清空 file_snapshot 条目（cause=reset） |

已知限制：file-approval 事件信封不带 session id（serve pump 用 subs 表会话
id 兜底注入 EventBus 事件，pump 登记路径条目 sessionId 正确；镜像兜底路径
条目 sessionId=""）；同会话多个不同文件集会并存多条，per-path resolved 时
整条收口，下轮 gate_check 重发即重建。

## 2. 条目形状（ApprovalEntry）

`src/approval_bus.rs`。序列化 camelCase（对齐 RPC 响应惯例）；kind 值
`ui_ask` / `file_snapshot` / `remote_verb`；`workerId` / `expiresAtMs` 为空时
**省略不序列化**（对齐 schemas/rpc/subscribe/approvals_pending.json 的
additionalProperties=false + string/integer 类型）。

```json
{
  "id": "apr_0f307bd1",
  "kind": "file_snapshot",
  "sessionId": "sess_x",
  "workerId": "wkr_1",
  "summary": "1 file(s) pending review: bus_ci.txt",
  "payload": {
    "nativeRequestId": "appr_1789574124216",
    "total": 1,
    "files": [{"path": "bus_ci.txt", "status": "added", "diffStat": "bus_ci.txt | 1+"}]
  },
  "raisedAtMs": 1789574124222
}
```

- `id`：统一前缀 `apr_<8hex>`（区别于 file-approval 原生 `appr_<ts>`）
- `payload.nativeRequestId`：路由回来源所需的原生 id（BusBackedSink 注册时注入）
- **内存态不落盘**（存储落位原则：审批是瞬态，宁丢不建文件；file-snapshot 的
  持久恢复仍归它自己的 session JSONL custom 条目机制）

幂等：同 dedupe key 且未过期的重复注册返回既有 id；过期条目惰性清理且不
阻塞同 key 重注册。

## 3. 单一路由：approval_respond（`WorkerRegistry::execute_unified_approval`）

RPC `approval_respond` 与审批泵（`auto_respond`）**共用**这一个执行入口
（`src/worker_registry.rs`）。`request_id` 接受统一 id（`apr_`）或来源
native id（`req_`/`vapp_`/`appr_`——泵按事件 native id 寻址）。

### 路由矩阵（终态）

| kind | 分支判定 | 通路 |
|------|---------|------|
| `ui_ask` | **host 进程 `pending_ui` 命中**（host 级 Ask） | oneshot send `"allow"`/`"deny"`（等价 `ui_respond`；AskResolved 由 Ask 发起方广播） |
| `ui_ask` | 未命中，条目带 workerId | `send ask_respond` 给来源 worker（worker 从自己的 pending_ui 放行工具） |
| `remote_verb` | — | 直取 verb 表 + oneshot 投递（等价 `verb_review` 语义） |
| `file_snapshot` | 条目带 workerId | `send review_approve_all` / `review_reject_all` 给来源 worker |
| 任一 | 条目无 worker 且来源侧不可达 | 可诊断错误（见下表） |

成功 → 条目出表 + 广播 `ApprovalResolved`（`by`=user/pump）+ 返回
`{id, kind, decision, remaining}`（对齐 approval_respond.json 的
responseSuccess，additionalProperties=false）。

**失败矩阵：**

| 场景 | error |
|------|-------|
| 未知/已收口 id | `approval not found: apr_x (unknown, already resolved, or source removed)` |
| 缺 decision | `missing params.decision` |
| 非法 decision | `invalid decision '<v>' (expected approve \| reject)` |
| 缺 id | `missing params.id` |
| ui_ask 底层已失效 | `ui ask request not found or already expired: req_x` |
| remote_verb 底层已失效 | `verb approval not found: vapp_x (already resolved?)` |
| file_snapshot 无 live worker | `file_snapshot approval has no live worker (session "..."); resolve via review_approve / review_reject RPC on the session` |

安全边界：`approval_respond` 与 `verb_review` 同级——认证边界是 host socket 的
peercred 同 uid 校验（P0），**不要求** `ui_respond` 的同源 subscribe(ui:true)
绑定（桥接方两步接入正是设计目标）。`ui_respond` 旧通路的同源绑定保持不变。

## 4. RPC 接口规格

### 4.1 approvals_pending（Pull）

**请求：**
```bash
ion rpc --method approvals_pending
# 可选 session 过滤：
ion rpc --method approvals_pending --params '{"session":"sess_x"}'
```

**响应 JSON（成功）：**
```json
{"data":{"total":1,"requests":[{...ApprovalEntry...}],"pending":[{...同形...}]},"id":"rpc-client","success":true,"type":"response"}
```

`pending` 是 schema 契约名（schemas/rpc/subscribe/approvals_pending.json）；
`requests`/`total` 为 M1 既有字段向后兼容保留。无失败分支（空表 total=0）。

### 4.2 approval_respond（Respond）

**请求：**
```bash
ion rpc --method approval_respond --params '{"id":"apr_0f307bd1","decision":"approve","reason":"lgtm"}'
```

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `id` | string | 必填 | 统一审批 id（`apr_` 前缀；也接受 native id） |
| `decision` | string | 必填 | `"approve"` 或 `"reject"`（fail-closed：未知值拒绝） |
| `reason` | string | 无 | 可选备注（进 ApprovalResolved 事件不进来源） |

**响应 JSON（成功）：**
```json
{"data":{"id":"apr_0f307bd1","kind":"ui_ask","decision":"approve","remaining":0},"id":"rpc-client","success":true,"type":"response"}
```

**失败矩阵**见 §3。校验顺序 fail-closed：缺 decision → 非法 decision → 缺 id。

## 5. 事件规格（Push）

总线变化经 EventBus 广播（`extension="host"`、`route="ui"`；`subscribe --ui`
与 `subscribe`（subscribe_all）均可见）。data 顶层平铺契约字段（对齐
schemas/rpc/subscribe/events/*.json），兼容保留 `approval` 完整对象：

| customType | data（契约字段） | 触发 |
|-----------|----------------|------|
| `ApprovalRequest` | `id/kind/sessionId/workerId/summary/payload/raisedAtMs`（+`approval` 对象） | 新条目注册（幂等命中不触发） |
| `ApprovalResolved` | `id/kind/decision/sessionId/workerId/by/reason`（+`approval`） | 决定落地（by=user/pump/system） |
| `ApprovalRemoved` | `id/kind/cause`（reset/source_gone/expired）（+`approval`） | 非决定性移除 |

与既有事件的区分：worker file-approval 原生 `ApprovalRequest`
（ext=`file-approval`，data 含 files/requestId，无 kind=legacy 形态）与总线
事件（ext=`host`，data 含 kind + approval 对象）**共存**；桥接方按
`data.approval` 是否存在识别总线统一事件。审批泵直发的
`ApprovalResolved`（by=pump，含 result）与总线收口广播（by=pump）同帧序共存。

## 6. 审批泵（policy map + by=pump）

- **策略**：per-kind map（`{"file_snapshot":"auto","ui_ask":"ask",...}`）或
  scalar（`auto_approve`/`default`）；生效优先级 worker 覆盖 > host 覆盖 >
  出生档案。RPC `sandbox_policy` GET/SET（写路径严格校验 fail-closed）。
- **泵判定**：worker reader loop 在 `ApprovalRequest` 事件上 `pump_arm`
  （冷却键 = worker#kind，2s 冷却）→ 锁外 spawn `approval_pump_fire`。
- **分发**（`worker_registry.rs approval_pump_fire`）：
  - `file_snapshot` → `sandbox_pump_resolve`（review_approve_all + 总线
    mark_resolved **by=pump** + SandboxAutoApproved 遗留事件）
  - `ui_ask` / `remote_verb` → `auto_respond` → `execute_unified_approval`
    （by=pump；与 RPC 共用单一路由）
- **已知竞态**：泵 spawn 早于 serve pump 总线登记 → `auto_respond` 首投
  "approval not found" 时 300ms×2 退避重试（登记在同一批 stdout 事件里，
  毫秒级完成）。
- 3kind×3policy 放行矩阵：`sandbox_pool::tests::pump_matrix_per_kind_policy`
  + `worker_registry::tests::pump_arm_kind_matrix_from_map_policy`。

## 7. snapshot 水合（向后兼容）

`subscribe(session)` 的快照帧 `event.data` 含 `approvals` 全量统一表；原
`pendingApprovals`（host 级 UI 请求 id 列表）**原样保留**：

```json
{
  "worker": {...},
  "session": {...},
  "pendingApprovals": {"count": 0, "requests": []},
  "approvals": {"total": 1, "requests": [<ApprovalEntry>...]},
  "generatedAt": 1789574124222
}
```

## 8. 桥接方最小接入示例（subscribe + approval_respond 两步）

```bash
# 步骤 1：订阅 UI 事件流（长连接，收到待审批）
ion subscribe --ui
# → {"type":"ui_event","ui_type":"ApprovalRequest","extension":"host",
#    "data":{"kind":"file_snapshot","id":"apr_0f307bd1","summary":"1 file(s) ...",...}}

# 步骤 2：应答（另一连接或 IM 回调里）
ion rpc --method approval_respond --params '{"id":"apr_0f307bd1","decision":"approve"}'

# 刷新/重连时拉全量（Pull 兜底）
ion rpc --method approvals_pending
```

## 9. 既有 bug 修复记录（各批顺带）

- **serve pump 死分支**（M1）：`mtype == "extension_event"` 检查外层
  `msg["type"]`（恒为 "event"，死分支）导致 worker 审批族事件从未进
  EventBus。修正为按内层 `event.type` 判定。
- **pump 双处理去重**（集成批）：M1 死分支修复与 M2 包裹形处理对同一事件
  双登记 + 双广播。合并为唯一处理点（`UI_FAMILY_CUSTOM_TYPES` 白名单 =
  审批/Ask/verb 族 + Confirm/Prompt/Alert/Notif）。
- **file_snapshot 镜像条目归属**（M2）：pump 登记路径带 worker/session（镜像
  路径为兜底），应答路由由此可达 worker（review_approve_all）。
- **verb_review 双收口**（集成批）：统一路由直取 verb 表（不经 verb_review
  的内置 sink resolve），避免泵路径 by=pump 被先行的 by=user 收口吞掉。

## 10. CLI 测试指南（三合一）

完整用例：`tests/approval_bus_ci.sh`（每 Phase 独立 host：私有 HOME + 私有
ION_HOST_SOCKET + 私有 ION_SESSION_DIR；faux provider 驱动真实 host 进程）：

| Phase | Group | 覆盖 |
|-------|-------|------|
| 1（M1 核心） | G1-G9 | Pull 空表 / file_snapshot 登记+形状 / **统一 Respond 闭环**（approve→review_approve_all→归零）/ 旧 API 兼容+镜像收口 / 错误分支（schema 对齐）/ Push 事件流（ext=host + kind 平铺 + 共存）/ snapshot 水合 / 旧 RPC 兼容 |
| 2（M2 Ask 托管） | W1-W4 | CommandGuard 中危→worker Ask→统一表（workerId 非空=双登记 sink 胜出）/ approval_respond→ask_respond→工具真执行 / 条目消除 |
| 3（M3 泵+策略） | P1x-P4x | policy map 两形态+严格拒绝 / 泵实测（SandboxAutoApproved + ApprovalResolved by=pump + 总线同步收口）/ ask 对照轮不触发泵 + 统一 Respond 收尾 / kind 枚举与字段 |

schema 动态校验：`cargo test --test rpc_schema_subscribe_test
approval_bus`（真实响应帧过 M3 契约：approvals_pending response +
approval_respond 四种错误变体）。

配套单测：
- lib：`cargo test --lib approval`（approval_bus 13 + approval_sink 22，含
  BusBackedSink 双登记去重/三来源映射/per-path 精收）
- lib：`cargo test --lib unified_`（单一路由矩阵：host oneshot 闭环 / verb
  真表 / file_snapshot 无 worker 指引 / 未知 id / 非法 decision）
- bin：`cargo test --bin ion approval_bus_tests`（镜像矩阵 + snapshot 兼容）

CI 注意事项：
- config 需 `extensions.file-snapshot.enabled=true`（默认 disabled）且
  `global-memory.enabled=false`（否则 host 启动时 memory-agent 单例会抢跑
  faux 脚本第一行，把写入吸收进会话 baseline，审批 pending 变空）。
- faux 脚本每行被 worker 逐 LLM 轮消费；脚本行数要覆盖全部 prompt 轮次。
- file-snapshot 按 diff 判定：同路径同内容 = 无 diff = 无审批请求（faux 内容
  带随机后缀）。

## 11. 源码导航

| 文件 | 内容 |
|------|------|
| `src/approval_bus.rs` | 总线核心：ApprovalEntry/ApprovalKind/ApprovalBus/ApprovalActor（注册幂等/惰性过期/变化回调 resolve_as）+ 单测 |
| `src/approval_sink.rs` | ApprovalSink trait + **BusBackedSink**（单一数据源转发，dedupe/TTL/summary 与镜像同算法）+ RecordingSink（测试替身）+ try_register_from_worker_event + respond_route 纯函数 |
| `src/worker_registry.rs` | **`execute_unified_approval`**（RPC 与泵共用的单一路由）+ `auto_respond`（泵接线，登记竞态重试）+ `sandbox_pump_resolve`（file 路径 + 总线 by=pump 收口）+ pump_arm/approval_pump_fire + verb_gate 直登 |
| `src/bin/ion.rs`（审批总线段） | HOST_EVENT_BUS / mirror_ui_event_to_bus（镜像兜底）/ wire_approval_bus（Push 广播，schema 形状） |
| `src/bin/ion.rs`（handle_manager_command） | `approvals_pending` / `approval_respond` match 臂 + `verb_review` 镜像同步收口 |
| `src/bin/ion.rs`（serve pump） | worker extension_event 唯一处理点（登记 + 白名单广播） |
| `src/runtime.rs` | worker 模式 Ask stdout 上报（emit_ask_event_stdout）+ resolve_ask 四路径 |
| `src/worker_rpc.rs` | worker `ask_respond` 双臂（主循环 + agent.run 期间） |
| `schemas/rpc/subscribe/{approvals_pending,approval_respond}.json` + `events/{approval_request,approval_resolved}.json` | 协议契约（M3 固化，实现已对齐） |
| `tests/approval_bus_ci.sh` | 三合一集成验证（G/W/P 三相位） |
