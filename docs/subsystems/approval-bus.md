# Approval bus: 三来源统一审批表与单一路由

> **状态：现状快照** — 基准 commit ffb6d5c（2026-10-14，验证情况见 [Verification](#verification)）。

ApprovalBus 是 host 进程内的统一审批表——内存单例、不落盘（`ApprovalBus::global()`，[src/approval_bus.rs:251](../../src/approval_bus.rs#L251)）。三家族审批（worker UI Ask / file-snapshot review / 远程 verb）注册进同一张表，id 统一 `apr_<8hex>`，应答走单一路由 `approval_respond`。外部桥接方（webui / IM / 手机推送）两步接入：`subscribe` 收事件 + `approval_respond` 表决定。

本页讲总线的当前契约：注册与去重、墓碑、四分支路由、事件出口、审批泵。设计动机与 M1-M3 演进决策在 [APPROVAL_BUS](../design/APPROVAL_BUS.md)（本文不重复其推导）；file-snapshot 审批的业务语义（diff、per-path resolved、session JSONL 持久化）在 [FILE_SNAPSHOT](../design/FILE_SNAPSHOT.md)；verb 封闭动词表在 [REMOTE_WORKER](../design/REMOTE_WORKER.md)。总线自身永不落盘——裁决标准见 [AGENTS.md §存储落位原则](../../AGENTS.md)。

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| 统一表 | host 进程内存单例（`OnceLock`），三类审批条目唯一汇聚点；变化回调 `on_change` | `ApprovalBus` in [src/approval_bus.rs:213](../../src/approval_bus.rs#L213) |
| 条目形状 | `ApprovalEntry`（camelCase 序列化；`id`/`kind`/`summary`/`payload`/`raisedAtMs`；`workerId`/`expiresAtMs` 空则省略；`payload.nativeRequestId` 供路由回来源） | [src/approval_bus.rs:90](../../src/approval_bus.rs#L90) |
| 注册幂等与墓碑 | 同 dedupe key 存活返回既有 id；收口时写 30s 墓碑抑制镜像迟到重注册 | `register` in [src/approval_bus.rs:294](../../src/approval_bus.rs#L294)，`TOMBSTONE_TTL_MS` (30_000) in [src/approval_bus.rs:33](../../src/approval_bus.rs#L33) |
| 收口与归因 | resolve_as / mark_resolved_matching_as 带 actor（User/Pump/System）与 note；过期惰性清理并广播 Removed | [src/approval_bus.rs:440](../../src/approval_bus.rs#L440) / [src/approval_bus.rs:526](../../src/approval_bus.rs#L526) |
| 来源侧窄接口 | `BusBackedSink`（生产）/ `RecordingSink`（测试）/ `NoopSink`（未装兜底）；`try_register_from_worker_event` 是"哪些事件算审批来源"的知识收敛点 | [src/approval_sink.rs:496](../../src/approval_sink.rs#L496) |
| 单一路由 | `execute_unified_approval` 四分支（host oneshot / worker ask_respond / verb 直取表 / review_approve_all）；id 兼容 `apr_` 与 native 双前缀 | [src/worker_registry.rs:4214](../../src/worker_registry.rs#L4214) |
| RPC 出口 | `approvals_pending`（可按 session 过滤）+ `approval_respond`（fail-closed 校验：缺 decision → 非法 decision → 缺 id） | [src/bin/ion.rs:7642](../../src/bin/ion.rs#L7642) / [src/bin/ion.rs:7668](../../src/bin/ion.rs#L7668) |
| Push 事件 | on_change → EventBus 广播 `ApprovalRequest` / `ApprovalResolved` / `ApprovalRemoved`；snapshot 帧 `data.approvals`（旧 `pendingApprovals` 保留） | `wire_approval_bus` in [src/bin/ion.rs:353](../../src/bin/ion.rs#L353)，镜像 in [src/bin/ion.rs:185](../../src/bin/ion.rs#L185) |
| 审批泵 | 无人值守审批停摆的自动放行：policy 生效链（worker 覆盖 > host 覆盖 > 出生档案）+ 冷却键 `(worker,kind)` | `pump_arm` in [src/worker_registry.rs:4147](../../src/worker_registry.rs#L4147)，`PUMP_COOLDOWN_MS` (2_000) in [src/sandbox_pool.rs:583](../../src/sandbox_pool.rs#L583) |
| 旧 API 兼容 | `verb_review` / `verb_pending` / `ui_respond` / `review_*` 全保留，完成后镜像收口进总线 | [src/bin/ion.rs:7707](../../src/bin/ion.rs#L7707) 起 |

truth 分层：总线是"当前 pending 什么"的唯一表；**持久决策归各来源**——file-snapshot 的审批状态持久在 session JSONL custom 条目（`persist_approval`，src/file_snapshot/approval.rs），Ask 与 verb 是瞬态。host 进程退出则总线归零，这是设计（宁丢不建 sidecar）。

## Runtime path

1. **Register.** 三来源进一张表：worker Ask 与 file-snapshot 经 serve pump 收 worker stdout 事件同步登记（`try_register_from_worker_event`，[src/approval_sink.rs:496](../../src/approval_sink.rs#L496)）；verb 走双登记（host 直登 [src/worker_registry.rs:4976](../../src/worker_registry.rs#L4976) + EventBus 镜像）；host 级 Ask / verb 原始事件由镜像兜底（`mirror_ui_event_to_bus`，[src/bin/ion.rs:185](../../src/bin/ion.rs#L185)）。pump 登记先于镜像，带 worker/session 的条目胜出，镜像随后 dedupe 命中不覆盖。Covered by `bus_backed_sink_dual_registration_dedupes`.
2. **Dedupe.** dedupe key 按 kind 对齐：`ask:<request_id>`（TTL 120s，对齐 Ask 等待窗）/ `verb:<requestId>`（300s）/ `fs:<path:status 签名>`（无 TTL，同文件集重发幂等）。同 key 存活 → 返回既有 id 不建新条目。Covered by `register_is_idempotent_on_dedupe_key`.
3. **Expose.** `on_change` 回调把 Registered/Resolved/Removed 转成 EventBus 三事件（含 `by` 归因与 `reason`），并水合 snapshot 帧 `data.approvals`。Covered by `snapshot_data_contains_unified_approvals_field`.
4. **Respond.** `approval_respond` fail-closed 校验后进 `execute_unified_approval`：① UiAsk 且 host `pending_ui()` 命中 → oneshot 直发；② UiAsk 其余 → `ask_respond` 命令送 worker（双臂注册：空闲主循环 + agent.run 期间 bg 通道）；③ RemoteVerb → 直取 verb 表发送（不调旧 `verb_review()`，保 by=pump 语义）；④ FileSnapshot → `review_approve_all` / `review_reject_all` 命令。统一 `resolve_as` 收口，响应 `{id, kind, decision, remaining}`。Covered by `unified_ui_ask_host_oneshot_closes_loop`, `unified_remote_verb_via_real_table`.
5. **Pump.** worker 事件流里 `customType == "ApprovalRequest"` → kind 解析（`data.kind` 缺省/未知 → FileSnapshot，legacy 兼容）→ policy 判定 → `pump_arm` 冷却检查 → `auto_respond`（[src/worker_registry.rs:4304](../../src/worker_registry.rs#L4304)）或 `sandbox_pump_resolve`（:4357），actor 恒为 Pump 且总线同步收口。Covered by `pump_arm_kind_matrix_from_map_policy`, `pump_arm_cooldown_is_per_kind`.
6. **Degrade.** 旧通路完成后由镜像收口进总线（`AskResolved` / `ApprovalResolved` / verb 超时各映射到 mark_resolved 或 remove），旧调用方不感知总线存在。Covered by `mark_resolved_matching_syncs_old_api_path`.

## Constraints and failure behavior

- 总线是 host 进程内存单例，never 落盘、never 跨进程共享；worker 进程看不到总线，只经 stdout 事件间接镜像。"pending 什么"以总线为准，"批过什么"以各来源持久层为准。
- 注册幂等靠 dedupe key：同 key 存活 → 同 id；收口写 30s (30_000ms) 墓碑，窗口内同 key 且 `worker_id` 为空的镜像重注册被吞（带 worker 的主路径放行并清墓碑）——治"泵收口后镜像兜底迟到重注册"竞态。Covered by `tombstone_suppresses_mirror_reregister_after_resolve`.
- 路由矩阵是封闭四分支，分支内 never 串旧通路：verb 分支直取 `verb_approvals_global` 表而不调 `verb_review()`——后者内置 `sink().resolve` 会以 by=user 抢先收口，破坏归因。
- 泵路径 actor 恒 Pump，且总线同步收口、不等 per-path `ApprovalResolved` 事件回流（避免 user 归因错标）。
- `approval_respond` fail-closed：缺 decision / 非法 decision / 缺 id 按序全拒（`ApprovalDecision::parse`）；`ui_respond` 同源绑定（须先 `subscribe` ui:true）。
- 条目查找兼容双前缀：`apr_<hex>` 总线 id 或 native id（`req_` / `vapp_` / `appr_`）——旧桥接方拿 native id 也能应答。
- TTL 语义分 kind：ui_ask 120s / remote_verb 300s（对齐各自等待窗，惰性过期广播 Removed）；file_snapshot 无 TTL——同签名幂等 + per-path resolved 精收 + `ApprovalReset` 以 cause=reset 收口。worker 死亡后的残留 file_snapshot 条目在 respond 时报可诊断错误（附 review_approve 指引），never 静默成功。
- 泵冷却键 `(worker,kind)`、窗口 2s (2_000ms)；policy 生效链 worker 覆盖 > host 覆盖 > 出生档案 `approval_policy`；`data.kind` 缺省/未知回落 FileSnapshot，never 拒绝。
- 过期条目惰性清理且不阻塞同 key 重注册；host 重启总线归零后，worker 侧重发的同签名事件幂等重建，never 产生重复条目。

## An approval that outlives its worker

worker 崩溃后其 pending 条目的三种收尾：ui_ask 靠 TTL 120s 过期（惰性清理广播 Removed）；remote_verb 靠 verb oneshot 的 300s 超时 spawn 任务主动 `sink().resolve(id, "timeout")`；file_snapshot 条目无 TTL、也不被 serve pump 主动清除（pump 只清死 worker 的订阅通道）——用户应答时走 `approval_worker_missing_error`（"no live worker → resolve via review_approve/review_reject on the session"，[src/worker_registry.rs:9933](../../src/worker_registry.rs#L9933)），指引改走会话级旧通路。Before, 三家族互不相通时外部桥接方无从知晓 worker 已死，应答打进空气（AGENTS.md 2026-09-16 修复批 7 记录）。host 重启则总线整体消失；file-snapshot worker 重新 spawn 后 `restore_from_session` 重发 `ApprovalRequest`，同签名幂等重建。Covered by `unified_file_snapshot_without_worker_errors_with_guidance`.

## Verification

```sh
cargo test --lib -- approval_bus::tests approval_sink unified pump_arm test_m2_verb
bash tests/approval_bus_ci.sh
```

本次快照（commit ffb6d5c，2026-10-14）实测：lib 过滤集合 41/41、`approval_bus_ci.sh` 三合一 **70 passed / 0 failed**（Phase 1 M1 核心 / Phase 2 M2 worker Ask 托管 / Phase 3 M3 policy map + 泵）。环境前提：CI 起 3 个 host 实例（私有 socket + 私有 HOME，不碰用户 host），M2 段含 CommandGuard 中危触发，M3 段用 mock `ION_REMOTE_WORKERS`；schema 契约另有 `tests/rpc_schema_subscribe_test.rs::approval_bus_rpc_frames_match_schemas`（本次 not run）。
