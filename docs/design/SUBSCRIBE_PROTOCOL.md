# Subscribe 协议升级 — epoch 栅栏 + 快照先行 + 版本握手 + 审批同源绑定

> **状态：已完成** — 四项协议能力已实现并通过单元测试（bin/ion.rs `subscribe_protocol_tests`）+ raw socket CI（`tests/subscribe_protocol_ci.sh`）。

## 0. 背景与目标

对标 pi 新版 protocol 包的三条纪律，**用现有 JSON 行协议（Unix socket + JSONL）实现，不引入新传输层**：

| # | 纪律 | 解决的问题 |
|---|------|-----------|
| 1 | **epoch 路由栅栏** | worker 重派/重绑后，旧订阅还在收旧 worker 残留帧（双写期混淆、重订阅竞态残留） |
| 2 | **快照先于增量（水合纪律）** | 新终端 subscribe 连上后是空白的，要等下一轮事件才知道状态 |
| 3 | **协议版本握手** | 客户端无法探测 host 支持的协议版本（可选能力，旧客户端完全不受影响） |
| 4 | **审批同源绑定** | `ui_respond` 之前任何连接都能答审批——安全漏洞（跨连接劫持审批） |

实现位置：`src/bin/ion.rs`（socket accept/subscribe/ui_respond 区域 + `SessionRouter`）。

## 1. 协议设计要点

### 1.1 epoch 语义

- host 维护 **per-session epoch**（u64，host 级全局表，首次见到该 session = 1）。
- **worker 重派/重绑该 session 时 epoch+1**：router 挂接的 worker 事件流（rx）结束时（worker 死亡/被 GC），router 统一推进 epoch、给所有旧 epoch 订阅者各发一条 `stale_route`、再重绑新 worker（最多等 10s；绑不上则 router 退出——旧订阅已全部作废）。
- epoch 表是 **host 级全局**（不随 router 退出丢失）：kill 后 session 重建（prompt 自动拉新 worker），新订阅 epoch 从 2 继续，保证单调。
- 订阅 ack 与后续每个转发事件都携带 epoch：

```json
{"type":"subscribed","session":"sess_x","stream":"instance","epoch":1,"replayed":0}
{"type":"instance_event","session":"sess_x","epoch":1,"event":{"type":"text_delta","delta":"..."}}
```

- epoch 推进后，旧 epoch 订阅收到一条 `stale_route` 并**停止转发**（信道关闭 → 连接断开）；客户端凭新 epoch 重新 subscribe：

```json
{"type":"stale_route","customType":"stale_route","session":"sess_x","epoch":1,"currentEpoch":2}
```

- 实现：per-session **SessionRouter** 任务（懒创建、至多一个），所有该 session 的 instance 订阅共用；router 独占 registry 订阅 rx 并做带 epoch 章的扇出。多连接订阅同 session 时，worker 重绑只发生一次 epoch 推进（对比旧实现每个连接各自重接）。

### 1.2 snapshot 字段清单（水合纪律）

subscribe(session) 建立后，host 在**任何 replay/实时增量之前**先推一条快照帧：

```json
{
  "type": "instance_event",
  "session": "sess_x",
  "epoch": 1,
  "snapshot": true,
  "event": {
    "type": "extension_event",
    "extension": "host",
    "customType": "snapshot",
    "visibility": "ui_only",
    "session": "sess_x",
    "data": {
      "worker": {
        "workerId": "w-1",
        "status": "Busy",
        "model": "glm-5.2",
        "agent": "build"
      },
      "session": {
        "sessionId": "sess_x",
        "model": "glm-5.2",
        "provider": "zai",
        "name": "demo"
      },
      "pendingApprovals": {
        "count": 2,
        "requests": ["req-1", "req-2"]
      },
      "generatedAt": 1757900000000
    }
  }
}
```

| 字段 | 来源 | 说明 |
|------|------|------|
| `worker.workerId` / `status` / `model` / `agent` | WorkerRegistry（短锁直读） | `worker` 为 `null` = 当前无存活 worker |
| `session.model` / `provider` / `name` | SessionIndex（直读磁盘） | 会话热字段 |
| `pendingApprovals.count` / `requests` | host 级 `pending_ui()` 表 | 待处理的 UI 审批请求（request_id 列表） |
| `generatedAt` | 系统时钟 | Unix ms |

客户端判据：**`.snapshot == true`（或 `.event.customType == "snapshot"`）为快照帧；此帧之前收到的只有 subscribed ack；此帧之后才是 replay/实时增量。**

### 1.3 hello 握手（可选，向后兼容）

```
→ {"id":"h1","method":"hello"}
← {"type":"response","id":"h1","success":true,
   "data":{"protocolVersion":1,"hostId":"3f2b8c6a-1d4e-4f50-9a1b-2c3d4e5f6078"}}
```

- hello **不消费连接**：回包后连接保持，客户端可继续 subscribe / ui_respond / RPC。
- 不发 hello 的旧客户端行为完全不变（CLI `ion rpc` / `ion subscribe` 零改动照常工作）。
- 版本常量：`PROTOCOL_VERSION = 1`（ion-protocol crate）。
- **hostId（P0.3，对标 pi `ServerHello.serverId`）**：host 进程的**逻辑实例身份**。
  进程首次被问到时生成规范小写 UUIDv4（`ion_protocol::host_id()`，`/dev/urandom`
  随机源 + 时间/pid LCG 兜底），**内存态 OnceLock、不落盘**（按存储落位原则
  "宁可丢也不建新文件"）——重启即换新身份；同一 host 进程内跨连接稳定。
- **客户端 pin 校验（可选）**：环境变量 `ION_EXPECT_HOST_ID=<uuid>` 设置时，`ion rpc`
  客户端在发正式请求**之前**先在同一条连接上发 hello 比对 `data.hostId`——不匹配
  （或缺字段）则报错断开（exit 1），防止连到陈旧 socket（旧 host 未死净 / 重启窗口）
  背后的"错误 host"。不设置则零开销、行为不变。`ion rpc --method hello` 的响应本身
  就含 hostId，用于采集当前值写入 `ION_EXPECT_HOST_ID`。

### 1.4 审批同源绑定

- `subscribe {ui:true}` 在**同一连接**上建立后，该连接的 `ui_respond` 才被放行（ui 事件流内并发读命令）。
- 未订阅 UI 的连接发 `ui_respond` 一律拒绝：

```json
{"type":"response","id":"u1","success":false,
 "error":"ui_respond rejected: requires a prior subscribe {ui:true} on the same connection"}
```

- 同连接放行后业务语义不变：request 不存在/已过期仍是 `request not found or already expired`。
- ⚠️ **行为变更**：旧模式"一个连接 `subscribe --ui` 收事件、另一个连接 `ion rpc ui_respond` 应答"将不再可用——UI 网关/客户端必须在**同一条连接**上完成订阅与应答。

## 2. RPC / 帧规格汇总

| 方法 | 方向 | 请求示例 | 响应/帧 |
|------|------|---------|---------|
| `hello` | C→S（可选，可反复） | `{"id":"h1","method":"hello"}` | `{"type":"response",...,"data":{"protocolVersion":1,"hostId":"<uuid-v4>"}}` |
| `subscribe`（session） | C→S | `{"id":"s1","method":"subscribe","session":"sess_x","replay":3}` | ack（带 epoch）→ snapshot 帧 → replay 帧（`replayed:true`）→ 实时 `instance_event`（带 epoch）→（重绑时）`stale_route` → 断开 |
| `subscribe`（ui） | C→S | `{"id":"u2","method":"subscribe","ui":true}` | `{"type":"subscribed","stream":"ui"}` → `ui_event` 流；同连接可发 `ui_respond`/`hello` |
| `subscribe`（extension/全部） | C→S | `{"method":"subscribe","extension":"memory"}` | 行为不变（未纳入 epoch 栅栏范围） |
| `subscribe_overview` | C→S | 不变 | 不变 |
| `ui_respond` | C→S | `{"method":"ui_respond","params":{"request_id":"r","response":"allow"}}` | 同源放行：成功/`not found`；未同源：`ui_respond rejected: ...` |

兼容性说明：
- ack 新增 `epoch` 字段、转发帧新增 `epoch` 字段、新增 `snapshot:true` 帧与 `stale_route` 帧——均为**增量字段/新帧类型**，旧解析器按未知字段/未知帧忽略即可。
- `resubscribed` 事件不再出现：worker 重绑改为 `stale_route` + 断开（旧订阅显式作废），客户端重订拿到新 epoch。
- replay 仅在 router **首次挂接**时生效（router 已挂接后的后续订阅者无法补历史，ack `replayed:0`）。

## 3. CLI 测试指南

### Group A: hello 握手

#### A1 握手返回版本 + hostId
```bash
python3 - << 'PY'
import socket, json
s = socket.socket(socket.AF_UNIX); s.connect("/tmp/ion_ci.sock")
s.sendall(b'{"id":"h1","method":"hello"}\n')
print(s.recv(65536).decode())
PY
```
**验证点：**
- ✅ `data.protocolVersion == 1`
- ✅ `data.hostId` 是规范小写 UUIDv4（36 位，version 位 4，variant 位 8/9/a/b）
- ✅ 连接保持：随后发 subscribe 仍能收到 ack

#### A2 hostId 稳定与重启换新（pin 语义）
```bash
ion rpc --method hello | jq -r '.data.hostId'   # 采集当前 hostId → ION_EXPECT_HOST_ID
ION_EXPECT_HOST_ID=<旧值> ion rpc --method list_sessions   # host 未重启 → 正常
# kill host → ion serve 重启后：
ION_EXPECT_HOST_ID=<旧值> ion rpc --method list_sessions   # → pin 不匹配报错断开（exit 1）
```
**验证点：**
- ✅ 同一 host 进程内多次 hello（含跨连接）hostId 相同
- ✅ host 重启后 hostId 变化；`ION_EXPECT_HOST_ID` 指向旧值时客户端报 "hostId pin 不匹配" 并断开
- ✅ 不设 `ION_EXPECT_HOST_ID` 时行为与旧版完全一致

### Group B: 快照先行 + epoch

#### B1 订阅先收快照
```bash
ion rpc --method create_worker --params '{"relation":"child","project_path":"/tmp/p","initial_prompt":"x"}'
# 用 raw socket 发 {"method":"subscribe","session":"<sid>"} 并逐行打印
```
**验证点：**
- ✅ 第 1 帧 `type=subscribed` 且带 `epoch`
- ✅ 第 2 帧 `.snapshot==true` 且 `event.customType=="snapshot"`
- ✅ 快照含 `worker` / `session.model` / `pendingApprovals`
- ✅ replay 帧（若带 replay 参数）在快照之后、增量之前

#### B2 重绑栅栏
**验证点：**
- ✅ `kill_worker` 后旧订阅收 `{"type":"stale_route","epoch":1,"currentEpoch":2}`，且它是最后一帧（随后断开）
- ✅ session 重建后新订阅 ack `epoch==2`
- ✅ 多连接订阅时只收一次 stale_route（router 统一重绑）

### Group C: 审批同源

#### C1 未订阅拒绝 / 同连接放行
**验证点：**
- ✅ 新连接直发 `ui_respond` → `ui_respond rejected: ...`
- ✅ 同一连接先 `subscribe {ui:true}` 再 `ui_respond`（unknown id）→ `request not found...`（说明过了同源闸）

### Group D: 旧客户端回归

**验证点：**
- ✅ `ion rpc list_sessions`（不发 hello）照常
- ✅ `ion subscribe --session <sid>` 照常，增量打印 epoch/snapshot 帧

自动化：`bash tests/subscribe_protocol_ci.sh`（隔离三件套：私有 HOME + ION_HOST_SOCKET + ION_SESSION_DIR，精确 PID 清理）。

## 4. 单元测试

`src/bin/ion.rs` `mod subscribe_protocol_tests`（`cargo test --bin ion subscribe_protocol`）：

| 测试 | 覆盖 |
|------|------|
| `epoch_first_seen_is_one` / `epoch_advance_increments` / `epoch_sessions_are_independent` | epoch 推进逻辑 |
| `stale_route_event_carries_old_and_new_epoch` | stale_route 帧字段 |
| `snapshot_data_contains_worker_session_pending` / `snapshot_data_without_worker_is_null_worker` | 快照组装 |
| `snapshot_event_envelope_has_customtype_snapshot_before_live` | 快照信封（customType/epoch） |
| `stamp_instance_event_keeps_event_and_adds_epoch` | 转发帧盖章 |
| `hello_reply_returns_protocol_version` / `hello_reply_tolerates_missing_id` / `host_id_stable_within_process_and_canonical` | hello 握手 + hostId（bin 侧） |
| `hello_reply_shape` / `hello_reply_carries_host_id` / `generate_host_id_is_canonical_lowercase_uuid_v4` 等（ion-protocol crate） | 信封形状 + hostId 生成纪律 + `sanitize_error` 脱敏 |
| `ui_respond_rejected_without_prior_ui_subscribe` / `ui_respond_allowed_after_ui_subscribe_on_same_connection` | 同源绑定 |

## 5. 已知边界与风险

1. **`ui_respond` 跨连接调用被拒**（本设计的重点）：`tests/p3_ui_ci.sh` 的旧写法（独立 `ion rpc ui_respond`）会失败，需要改为同连接 subscribe+respond；ion-webui 网关若按连接池直调也需同源改造。
2. **router 首挂 60s 等待是串行点**：该 session 的首个 subscribe 触发挂接期间，同 session 的其他 subscribe 命令排队（旧行为是各连接并行等）。实际影响小（挂接完成后秒级放行）。
3. **epoch 是 host 进程内状态**：host 重启后从 1 重新计数（不持久化——按存储落位原则，宁可丢也不建新文件）。客户端不应跨 host 重启比较 epoch。
4. **pendingApprovals 是 host 级 UI 请求**：worker 侧 review 类待审批不在此列（需 worker RPC，快照组装不做同步 RPC 调用）。
5. **hostId 与 epoch 同理是进程内存态**（不落盘）：host 重启后 hostId 必然变化——`ION_EXPECT_HOST_ID` pin 到旧值在重启后失败是**预期行为**（防连错正是它的职责），客户端需重新 hello 采集新值。
6. **错误响应统一脱敏**（P0.2，ion-protocol `sanitize_error` 收口）：所有 RPC 错误 `error` 字段经 panic/backtrace 细节收敛（固定文案 `internal error (details redacted)`）+ home 路径 `~` 化 + 500 字符截断；公共错误文案（Unknown command / session not found on disk / verb / 审批类）原样保留。
