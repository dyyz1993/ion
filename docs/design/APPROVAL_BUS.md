# 统一审批总线（Approval Bus）

> **状态：开发中** — M1 批次：总线核心 + host 出口（Pull/Respond/Push）已实现并全量验证；
> file_snapshot 与 worker Ask 的 **应答路由** 留给 M2 接入（镜像与出口已就绪）。

---

## 概览

ION 原有三套互不相通的审批家族，id / 条目形状 / 审批动词各不相同，外部桥接方
（webui / IM 机器人 / 手机推送）无法用一个协议接完：

| # | 家族 | 表 | 应答 RPC | 事件 |
|---|------|-----|---------|------|
| ① | host 级 UI Ask（permission 询问 / wasm ui_ask） | `runtime::pending_ui()`（oneshot 表） | `ui_respond`（同源绑定） | `Ask` / `AskResolved` / `AskTimedOut` |
| ② | worker 级 file-snapshot 审批 | worker 内 `ApprovalManager`（session JSONL 持久） | `review_approve` / `review_reject`（session 级） | `ApprovalRequest` / `ApprovalResolved` / `ApprovalReset` |
| ③ | host 级远程动词审批 | `worker_registry::verb_approvals_global()` | `verb_review` | `verb_approval` |

统一审批总线在 **host 进程** 维护一张统一审批表：三类来源注册进同一张表，条目形状统一
（`ApprovalEntry`），id 统一 `apr_<8hex>` 前缀。出口全部长在既有协议上（host socket
RPC / EventBus / subscribe snapshot），**旧 API 一律不动**。

| 能力 | 入口 | 状态 |
|------|------|------|
| Pull（全量统一表） | RPC `approvals_pending` | ✅ M1 |
| Respond（路由回来源执行） | RPC `approval_respond` | ✅ ui_ask + remote_verb；file_snapshot 留 M2 |
| Push（注册/移除/决定事件） | EventBus `ApprovalRequest` / `ApprovalResolved` / `ApprovalRemoved` | ✅ M1 |
| snapshot 水合（W2 升级） | `subscribe(session)` 快照帧 `data.approvals` | ✅ M1（向后兼容：`pendingApprovals` 保留） |
| 三来源镜像注册 | serve 内 EventBus 监听器 | ✅ M1（file_snapshot 镜像为只读） |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | 总线核心（注册幂等/查询/过期/回调） | ✅ | `cargo test --lib approval_bus`（12） |
| 1.2 | 应答路由矩阵（ui_ask/remote_verb 闭环，file_snapshot 明确 M2 错误） | ✅ | `cargo test --bin ion approval_bus_tests`（12） |
| 1.3 | serve pump 死分支修复（worker 审批族事件进 EventBus） | ✅ | `tests/approval_bus_ci.sh` G7.2 |
| 2.1 | file_snapshot 来源应答路由（M2 接 `review_approve`） | 🔧 M2 | `approval_respond` 返回明确错误 |
| 2.2 | worker 侧 Ask 进 host 托管表（M2） | 🔧 M2 | 见 §7 钩子清单 |

---

## 1. 三来源映射表

| 来源 kind | 注册方式（M1） | dedupe key | TTL | native id（payload.nativeRequestId） | 应答路由 |
|-----------|---------------|-----------|-----|--------------------------------------|---------|
| `ui_ask` | EventBus `Ask`（ext=ui）镜像 | `ask:<request_id>` | 120s（对齐 Ask 等待窗） | `req_<hex>`（pending_ui 键） | `pending_ui().remove(id)` → oneshot send `"allow"`/`"deny"`（等价 ui_respond） |
| `remote_verb` | EventBus `verb_approval`（ext=session）镜像 | `verb:<requestId>` | 300s（对齐 verb 等待窗） | `vapp_<hex>` | `verb_review(native, approve)` |
| `file_snapshot` | EventBus `ApprovalRequest`（ext=file-approval，经 serve pump 转 ui 路由）镜像 | `fs:<paths:statuses 签名>` | 无（持续型） | `appr_<ts>`（file-approval 原生 requestId） | **M2**：未接线，返回明确错误 + 指引 `review_approve/review_reject` |

镜像的生命周期同步：

| 来源侧事件 | 总线动作 |
|-----------|---------|
| `Ask` | 注册 ui_ask |
| `AskResolved` / `AskTimedOut` | mark_resolved（approve=allow / reject，note=timeout） |
| `verb_approval` | 注册 remote_verb |
| 旧 `verb_review` RPC 成功 | mark_resolved（`handle_manager_command` 内同步收口） |
| `ApprovalRequest`（file-approval） | 注册 file_snapshot（同文件集重发幂等） |
| `ApprovalResolved`（file-approval，per-path） | 移除含该 path 的条目（Resolved） |
| `ApprovalReset`（file-approval） | 清空 file_snapshot 条目（Reset） |

**M1 已知限制**（M2 改进）：file-approval 事件信封不带 session id（pump 侧有但未注入），
镜像条目 `sessionId=""`；file_snapshot 的"最新通知即快照"语义——同会话多个不同文件集
会并存多条，per-path resolved 时整条收口，下轮 gate_check 重发即重建。

## 2. 条目形状（ApprovalEntry）

`src/approval_bus.rs`。序列化 camelCase（对齐 RPC 响应惯例）；kind 值
`ui_ask` / `file_snapshot` / `remote_verb`。

```json
{
  "id": "apr_0f307bd1",
  "kind": "file_snapshot",
  "sessionId": "",
  "workerId": null,
  "summary": "1 file(s) pending review: bus_ci.txt",
  "payload": {
    "nativeRequestId": "appr_1789574124216",
    "total": 1,
    "files": [{"path": "bus_ci.txt", "status": "added", "diffStat": "bus_ci.txt | 1+"}]
  },
  "raisedAtMs": 1789574124222,
  "expiresAtMs": null
}
```

- `id`：统一前缀 `apr_<8hex>`（区别于 file-approval 原生 `appr_<ts>`）
- `payload.nativeRequestId`：路由回来源所需的原生 id（各来源注册时约定写入）
- `raisedAtMs` / `expiresAtMs`：Unix ms；`approvals_pending` 按 raisedAtMs 升序
  （同毫秒按 id 字典序稳定排序）
- **内存态不落盘**（存储落位原则：审批是瞬态，宁丢不建文件；file-snapshot 的持久
  恢复仍归它自己的 session JSONL custom 条目机制）

幂等：注册携带 `dedupe_key`（来源侧稳定标识），同 key 且未过期的重复注册返回既有 id
（`newly_registered=false`）；过期条目惰性清理且不阻塞同 key 重注册。

## 3. RPC 接口规格

### 3.1 approvals_pending（Pull）

**请求：**
```bash
ion rpc --method approvals_pending
```

**请求参数：** 无（host 级命令，不带 session）。

**响应 JSON（成功）：**
```json
{"data":{"total":1,"requests":[{"id":"apr_0f307bd1","kind":"file_snapshot","sessionId":"","workerId":null,"summary":"1 file(s) pending review: bus_ci.txt","payload":{...},"raisedAtMs":1789574124222,"expiresAtMs":null}]},"id":"rpc-client","success":true,"type":"response"}
```

**响应 JSON（失败）：** 无失败分支（空表返回 `total:0`）。

### 3.2 approval_respond（Respond）

**请求：**
```bash
ion rpc --method approval_respond --params '{"id":"apr_0f307bd1","decision":"approve","reason":"lgtm"}'
```

**请求参数：**
| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `id` | string | 必填 | 统一审批 id（`apr_` 前缀） |
| `decision` | string | 必填 | `"approve"` 或 `"reject"`（fail-closed：未知值拒绝） |
| `reason` | string | 无 | 可选备注（进事件不进来源） |

**响应 JSON（成功）：**
```json
{"data":{"id":"apr_0f307bd1","kind":"ui_ask","decision":"approve","nativeRequestId":"req_ab12cd34"},"id":"rpc-client","success":true,"type":"response"}
```

**响应 JSON（失败矩阵）：**
| 场景 | error |
|------|-------|
| 未知/已收口 id | `approval not found: apr_x (unknown, already resolved, or source removed)` |
| file_snapshot（M2 未接线） | `file_snapshot approval routing not wired yet (planned M2): resolve via review_approve / review_reject RPC on the session (files: [...])` |
| ui_ask 底层已失效 | `ui ask request not found or already expired: req_x` |
| remote_verb 底层已失效 | `verb approval not found: vapp_x (already resolved?)` |
| 非法 decision | `invalid params.decision: expect "approve" or "reject"` |
| 缺 id | `missing params.id (unified approval id, apr_ prefix)` |

安全边界：`approval_respond` 与 `verb_review` 同级——认证边界是 host socket 的
peercred 同 uid 校验（P0），**不要求** `ui_respond` 的同源 subscribe(ui:true) 绑定
（桥接方两步接入正是设计目标）。`ui_respond` 旧通路的同源绑定保持不变。

**ui_ask 应答语义**：等价 `ui_respond`——取走 `pending_ui` oneshot 发
`"allow"`/`"deny"`，并广播 `AskResolved`（既有 UI 监听不受影响）+ `ApprovalResolved`。

## 4. 事件规格（Push）

总线变化经 EventBus 广播（`extension="host"`、`route="ui"`；`subscribe --ui` 与
`subscribe`（subscribe_all）均可见）。customType 沿用 K5 审批泵既有命名：

| customType | 触发 | data |
|-----------|------|------|
| `ApprovalRequest` | 新条目注册（幂等命中不触发） | `{"approval": <ApprovalEntry>}` |
| `ApprovalResolved` | 决定落地（approval_respond / 旧通路镜像同步） | `{"approval": <entry>, "decision": "approve"\|"reject", "reason": ...}` |
| `ApprovalRemoved` | 非决定性移除（reset / source_gone / expired） | `{"approval": <entry>, "cause": "reset"\|"source_gone"\|"expired"}` |

与既有事件的区分：worker file-approval 原生 `ApprovalRequest`（ext=`file-approval`，
data 含 files/requestId）与总线事件（ext=`host`，data 含 `approval` 对象）**共存**；
桥接方按 `data.approval` 是否存在识别总线统一事件。

## 5. snapshot 水合（W2 升级，向后兼容）

`subscribe(session)` 的快照帧 `event.data` 新增 `approvals` 全量统一表；原
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

新客户端一律读 `approvals`；jsonschema 契约（snapshot_frame.json）不含
additionalProperties 限制，新增字段兼容通过（rpc_schema_subscribe_ci 30/30）。

## 6. 桥接方最小接入示例（subscribe + approval_respond 两步）

```bash
# 步骤 1：订阅 UI 事件流（长连接，收到待审批）
ion subscribe --ui
# → {"type":"ui_event","ui_type":"ApprovalRequest","extension":"host",
#    "data":{"approval":{"id":"apr_0f307bd1","kind":"ui_ask","summary":"Ask: 允许执行 bash — which curl",...}}}

# 步骤 2：应答（另一连接或 IM 回调里）
ion rpc --method approval_respond --params '{"id":"apr_0f307bd1","decision":"approve"}'

# 刷新/重连时拉全量（Pull 兜底）
ion rpc --method approvals_pending
```

## 7. M2 接入钩子清单（本批留好挂点）

M2（worker 侧接入批）需要做的接线，挂点已就绪：

1. **file_snapshot 应答路由**：`bin/ion.rs route_approval_response` 的
   `ApprovalKind::FileSnapshot` 分支——把明确错误替换为按
   `payload.nativeRequestId` + sessionId 转发 `review_approve/review_reject`
   RPC（经 `WorkerRegistry::send_async` 到目标 worker）。条目 payload 已保留
   files/path 全量信息。
2. **worker 侧 Ask 进托管表**：worker 的 wasm `host_ui_ask` / SecuredRuntime
   `resolve_ask` 目前在 worker 进程内等待（pending_ui 在 worker 内存，
   host 的 ui_respond 够不着）。M2 需把 Ask 事件经 worker stdout 转发 host
   （serve pump 的审批族转发修复后链路已通），并在应答路由里把决定回传 worker
   （可复用 manager command 通道）。
3. **verb_approvals 注册接线**（如需主动注册而非事件镜像）：M1 用事件镜像，
   `worker_registry.rs` 未动；若 M2 想在 insert 时同步注册（拿到 host 侧
   ApprovalEntry id 回填），需在 `verb_gate_execute` 的 ask_on_deny 分支加
   `ion::approval_bus::ApprovalBus::global().register(...)` 调用。
4. **file-approval 事件带 session id**：`src/file_snapshot/approval.rs
   emit_public_event` 目前不带 session 字段；serve pump 已用 subs 表会话 id
   兜底注入 EventBus 事件，但总线镜像条目 sessionId 仍为空——M2 在源头补
   session 字段即可让条目归会话。

## 8. 发现并修复的既有 bug（M1 顺带）

**serve pump 死分支**（`bin/ion.rs` 事件泵）：`mtype == "extension_event"` 检查的是
外层 `msg["type"]`（实际值恒为 `"event"`，扩展事件类型在 `msg["event"]["type"]`），
导致 worker 审批族事件（file-approval ApprovalRequest 等）**从未广播到 EventBus**——
`subscribe --ui` 一直看不到 file-snapshot 审批（file_snapshot_ci K 组断言长期走
skip 掩盖）。M1 修正为按内层 `event.type` 判定，且只转发审批/Ask 族（route=ui），
其余扩展事件维持原状不上 EventBus（避免噪声面扩大）。

## 9. CLI 测试指南

完整用例：`tests/approval_bus_ci.sh`（隔离三件套：私有 HOME + 私有
ION_HOST_SOCKET + 私有 ION_SESSION_DIR；faux provider 驱动真实 host 进程）。

| Group | 覆盖 |
|-------|------|
| G1 | Pull 空表形状 |
| G2 | file_snapshot 来源镜像（faux write → ApprovalRequest → apr_ 条目） |
| G3 | 条目形状（前缀/kind/summary/payload.files/raisedAtMs） |
| G4 | Respond 矩阵（file_snapshot → M2 错误 + 条目保留） |
| G5 | 旧 API 兼容（review_pending/review_approve）+ 镜像同步收口 |
| G6 | 错误分支（未知 id/非法 decision/缺 id） |
| G7 | Push（subscribe --ui 收总线 ApprovalRequest(ext=host) + 原生事件共存 + ApprovalResolved） |
| G8 | snapshot 水合（approvals 全量非空 + pendingApprovals 兼容） |
| G9 | 旧 RPC 兼容（verb_pending/verb_review/ui_respond 同源绑定不变） |

实测 28/28。配套：lib 单测 12（`cargo test --lib approval_bus`）、bin 单测 12
（`cargo test --bin ion approval_bus_tests`，含 ui_ask approve/reject 真实
pending_ui oneshot 闭环 + remote_verb 注入式后端闭环）。

CI 注意事项：
- config 需 `extensions.file-snapshot.enabled=true`（默认 disabled）且
  `global-memory.enabled=false`（否则 host 启动时 memory-agent 单例会抢跑 faux
  脚本第一行，把写入吸收进会话 baseline，审批 pending 变空）。
- faux 脚本每行被 worker 逐 LLM 轮消费；脚本耗尽后 prompt 会挂 auto-retry——
  脚本行数要覆盖全部 prompt 轮次。

## 10. 源码导航

| 文件 | 内容 |
|------|------|
| `src/approval_bus.rs` | 总线核心：ApprovalEntry/ApprovalKind/ApprovalBus（注册幂等/惰性过期/变化回调）+ 12 单测 |
| `src/bin/ion.rs`（审批总线段） | HOST_EVENT_BUS 把手 / mirror_ui_event_to_bus 镜像 / wire_approval_bus 接线 / ApprovalRespondDeps + route_approval_response 路由矩阵 |
| `src/bin/ion.rs`（handle_manager_command） | `approvals_pending` / `approval_respond` match 臂 + `verb_review` 镜像同步收口 |
| `src/bin/ion.rs`（serve pump） | 审批族事件转 EventBus 的死分支修复 |
| `src/bin/ion.rs`（build_snapshot_data） | snapshot `approvals` 字段 |
| `tests/approval_bus_ci.sh` | 28 case 集成验证（隔离三件套） |
