# Worker supervision: heartbeat 判死与 AUTO-RECOVERY 重派

> **状态：现状快照** — 基准 commit ffb6d5c（2026-10-14，验证情况见 [Verification](#verification)）。

Worker supervision 是 host 侧的 worker 监护：heartbeat tick（默认 30s）巡检所有 worker，Idle 静默超 180s 标 Stale、Busy 静默超 600s 判 Dead 并触发 AUTO-RECOVERY 自动重派（[src/bin/ion.rs:7107](../../src/bin/ion.rs#L7107) 的 Background task 4）。

本页讲判死判据、死亡分类、重派门禁/配额/继承/沙盒故障转移的当前契约。崩溃诊断的捕获设计（stderr/exit code/父通知）与队列落盘回放、原地重建三档的演进记录在 [CRASH_RECOVERY](../design/CRASH_RECOVERY.md)（其 §0"不做自动重启"是早期哲学，现状以本页为准）；沙盒池探测协议在 [SANDBOX_POOL](../design/SANDBOX_POOL.md)；[AUTO_RECOVERY_COVERAGE](../testing/AUTO_RECOVERY_COVERAGE.md) 是 2026-09-12 时点的可靠性快照（其中"心跳超时未覆盖"等条目现已实现）。

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| 判死决策 | 纯函数：Idle 静默 >180s → Stale（软标记）；Busy 静默 >600s → Dead（终态）；Dead/Stale 不再变化 | `heartbeat_decision_with` in [src/worker_registry.rs:10097](../../src/worker_registry.rs#L10097)，`HEARTBEAT_IDLE_STALE_MS` (180_000) / `HEARTBEAT_BUSY_DEAD_MS` (600_000) in [src/worker_registry.rs:10075](../../src/worker_registry.rs#L10075) |
| 活性信号 | 任何 worker stdout 事件同权刷新 `last_heartbeat`（不只 text_delta）；agent_end/agent_stopped/error 转 Idle | `update_record_on_pump_event` in [src/worker_registry.rs:10118](../../src/worker_registry.rs#L10118) |
| heartbeat 循环 | tick 30s（`ION_HEARTBEAT_TICK_SECS` 可覆盖）+ 阈值 env 覆盖 + tick 末 `gc_workers(600)` + Idle 池上限 8（`ION_MAX_IDLE_WORKERS`） | [src/bin/ion.rs:7107](../../src/bin/ion.rs#L7107) |
| 死亡分类 | exit 四象限：Some(0)=Clean / Some(c)=Crashed / 信号=Signaled（SIGKILL/OOM）/ 双 None=Unknown 竞态兜底 | `classify_worker_exit` in [src/worker_registry.rs:10167](../../src/worker_registry.rs#L10167) |
| stdout-EOF 收尾 | Crashed/Signaled → 标 Dead + stderr 尾 10 行写 exit_reason + `child_crashed` 事件 + 父 worker 通知；Clean/Unknown → 移除记录 | [src/worker_registry.rs:3132](../../src/worker_registry.rs#L3132) |
| 重派门禁 | `(is_remote \|\| allow_local) && exit_code != Some(0) && session_id 非空`——正常完成 never 重派 | `auto_respawn_gate` in [src/worker_registry.rs:10190](../../src/worker_registry.rs#L10190) |
| 重派计划 | 三档策略（InPlacePrimary / InPlaceReflowScan / SummaryRelay）按本机/远端矩阵决策 | `decide_respawn_strategy` in [src/worker_registry.rs:10278](../../src/worker_registry.rs#L10278) |
| 配额 | 每 session 重派计数 3 次（原地重建与摘要接力同池）；耗尽返回 None 不再自动重派 | `auto_respawn_counts` in [src/worker_registry.rs:10449](../../src/worker_registry.rs#L10449) |
| 继承 | agent 取 record 原值（非硬编码）、model/provider 取 SessionIndex | `respawn_inherit_fields` in [src/worker_registry.rs:10528](../../src/worker_registry.rs#L10528) |
| 注入防劫持 | 重派任务卡只认 origin==user 的输入：steer/followUp/interrupt 一律判注入；按 `input_origin` custom 留痕 + textPreview 前缀匹配复核 | `generate_resume_prompt` in [src/worker_registry.rs:10643](../../src/worker_registry.rs#L10643) |
| 接力纪律 | 内核注入的重派 prompt 纪律：先 git 认领半成品 → 动手优先 → 小块即 commit | `RELAY_DISCIPLINE` in [src/worker_registry.rs:10306](../../src/worker_registry.rs#L10306) |
| 沙盒故障转移 | 池成员重派前 probe_all → pick_healthy 换节点；全死回落原 host + WARN；auto_recovered 事件带三态 failover | `respawn_host_failover` in [src/worker_registry.rs:10463](../../src/worker_registry.rs#L10463)，`pick_healthy` in [src/sandbox_pool.rs:222](../../src/sandbox_pool.rs#L222) |
| 本地开关 | `runtime.auto_respawn_local` 默认 false——本地 worker 异常死亡不自动重派（本地有 file-snapshot 回滚兜底） | [src/config.rs:484](../../src/config.rs#L484) |

truth 分层：worker 死没死以 host 视角的活性/EOF 证据为准（worker 自己永远不知道自己死了）；重派能否成立以 SessionIndex 的继承字段与会话 JSONL 的可重建历史为准。

## Runtime path

1. **Beat.** 两条 stdout 泵（[src/worker_registry.rs:2116](../../src/worker_registry.rs#L2116) / :2914）对每条 worker 事件调 `update_record_on_pump_event` 刷 `last_heartbeat`；agent_end/agent_stopped/error 顺带转 Idle。Covered by `test_bug1_all_event_kinds_are_liveness_signals`.
2. **Judge.** tick 30s 对每个 worker 跑 `heartbeat_decision_with`：Idle 静默 >180s 标 Stale；Busy 静默 >600s 先收集重派计划（`try_auto_respawn_gated`，[src/bin/ion.rs:7177](../../src/bin/ion.rs#L7177)）再标 Dead。判据是 `last_heartbeat`，never `status_since`。Covered by `test_heartbeat_decision_thresholds`.
3. **Classify.** stdout-EOF 收尾时 `classify_worker_exit` 分四象限；Crashed/Signaled 标 Dead、写 exit_reason、广播 `child_crashed`、通知父 worker；Clean/Unknown 清理移除。Covered by `test_bug2_exit_classification_matrix`.
4. **Gate.** `auto_respawn_gate` 三条件（远程或本地开关 / exit≠0 / 有 session）+ 每 session 配额 3 次；任一不满足 → 不重派。Covered by `test_f3_auto_respawn_gate_local_remote`, `test_k4_quota_counts_inplace_rebuild`.
5. **Failover.** `respawn_host_failover`：非池成员零开销原样返回；池成员 probe_all 后 pick_healthy——三态 `pool_pick`（原 host 仍健康选回）/ `pool_failover`（换节点，且原计划为原地重建时降级摘要接力）/ `fallback_no_healthy`（全死回落 + WARN）。Covered by `test_k4_failover_node_change_downgrades_inplace_to_relay`.
6. **Respawn.** 两处构造点（stdout-EOF [src/worker_registry.rs:3285](../../src/worker_registry.rs#L3285) + heartbeat [src/bin/ion.rs:7233](../../src/bin/ion.rs#L7233)）均 `max_turns: Some(200)`；继承 agent/model；`session: Some(sid)` + `relation: Child` 使新 worker 预载全历史并自动回放未消费的 queued_input（三闸幂等，协议见 [CRASH_RECOVERY §8](../design/CRASH_RECOVERY.md)）；接力纪律注入 prompt；广播 `auto_recovered`（trigger/failover/inplace/strategy，在 probe_all 之后）。Covered by `test_respawn_inherit_fields_takes_record_agent_and_index_model`, `heartbeat_ci.sh` Group C.

## Constraints and failure behavior

- 判死判据是 `last_heartbeat`（最后产出时刻），never `status_since`（进 Busy 时写一次的起点）——用后者会把长寿 run 在 spawn+600s 整点处决（`7f9ee4d` 修复，W1）。任何 stdout 事件都是活性信号，tool 结果与 text_delta 同权。
- Idle 静默只标 Stale，never Dead；Dead 是终态不可覆盖。`set_status` 幂等（同状态跳过），防 status_since 被重置成"永生僵尸"。
- 两处重派 `WorkerCreateConfig` 的 `max_turns` 必须一致为 200——host 默认 20（`ION_MAX_TURNS`，[src/worker_rpc.rs:444](../../src/worker_rpc.rs#L444)）会让重派 worker 几分钟烧完预算（`82fd407`，提交信息原文"此前漏传吃默认 20"）。
- 每 session 重派配额 3 次，原地重建与摘要接力同池；耗尽后 never 自动重派，须人工介入。
- 重派任务卡只认 origin==user：steer/followUp/interrupt 判注入，monitor/system 注入按 `input_origin` 留痕 + 前缀匹配复核——防会话注入劫持替补的任务（`b22d064`，W6）。Covered by `test_resume_prompt_skips_followup_injection`, `test_resume_prompt_skips_monitor_origin_message`.
- exit == Some(0) never 触发重派（正常完成）；本地 worker 默认不在门内（`auto_respawn_local` false）。
- `auto_recovered` 事件在 failover 探测之后发射（池探测通常 <5s）——never 先报恢复再发现目标节点已死。
- agent 继承取 record 原值 never 硬编码；model/provider 以 SessionIndex 为准（字段契约见 [session-index](./session-index.md)）。
- 换节点时原地重建降级摘要接力（远端文件不在新节点，本机 JSONL 摘要是保真度下限），WARN 记录降级原因。

## A long run that looks dead

长寿 run（长工具执行、长 LLM 生成、静默思考）在屏幕上看像挂死，但 heartbeat 只看最后产出——只要 stdout 还有任何事件，worker 永远活着；Busy >600s 判死仅覆盖真静默。Before, 判据是 `status_since`（进 Busy 起算），长寿 run 在 spawn+600s 整点被必然误杀，流水中段大批冤案（AGENTS.md 2026-09-15 交叉调研批次 W1 修复记录）；同年 09-14 流水线侧另有"206s STALL 自行回落"实证：长输出 ≠ 挂死，只告警不杀是正确设计。`heartbeat_ci.sh` Group C 用 12×sleep 1s 的纯工具事件流验证不误判（C3）。Covered by `test_bug1_tool_only_events_keep_busy_worker_alive`, `heartbeat_ci.sh`（Group C，faux worker 真实事件流）.

## Verification

```sh
cargo test --lib -- test_heartbeat_decision test_bug1 test_bug2 test_f3 test_k4 \
  test_respawn_inherit test_resume_prompt test_auto_recovery_skips
cargo test --test inplace_rebuild_harness
cargo test --test queue_persist_harness
bash tests/heartbeat_ci.sh
```

本次快照（commit ffb6d5c，2026-10-14）实测：lib 过滤集合 25/25、`inplace_rebuild_harness` 3/3、`queue_persist_harness` 5/5、`heartbeat_ci.sh` **PASS=14 FAIL=0**（Group A 纯函数 / Group B Stale 不越级 / Group C 真实 host + faux worker 工具流不误判，私有 HOME + 私有 socket）。环境前提：heartbeat_ci 调小阈值（`ION_HEARTBEAT_IDLE_MS=2s` / `ION_HEARTBEAT_BUSY_MS=8s`）加速判定，真实默认值见正文常量表；failover 无独立 CI shell，覆盖在 lib 纯函数单测（`sandbox_pool.rs` FailoverDecision 三态）；真实 LLM 的 T4（kill -9 → 同 sid 重建，`tests/e2e_2026_09_batches.rs`）not run（需 `ION_E2E=1`）。
