# Worker RPC command surface: 双臂命令分发与输出汇聚

> **状态：现状快照** — 基准 commit 98e02ca（2026-10-14，验证情况见 [Verification](#verification)）。

Worker RPC 是 worker 子进程的命令面：stdin/stdout JSONL 协议，`run_worker_rpc`（[src/worker_rpc.rs:129](../../src/worker_rpc.rs#L129)）驱动。host 通过 `current_exe() + --mode rpc` spawn 自身创建 worker（对齐 pi 的 `pi --mode rpc`），一进一出之间是 163 个空闲臂命令 + 约 16 个 run 期间 bg 臂命令。

本页讲命令面的契约：双臂分发、输出汇聚、执行域四命令（prompt/steer/abort/followUp）、append 白名单、spawn 协议。host 侧的路由与三场景引擎不在本页（直读/转发/合成的分野属 host 命令循环）；审批应答的路由回源在 [approval-bus](./approval-bus.md)；worker 的判死与重派在 [worker-supervision](./worker-supervision.md)；LLM 协议与流式重试在 ion-provider crate（[PROVIDER_PROTOCOL](../design/PROVIDER_PROTOCOL.md)）；协议信封定义在 ion-protocol（`MAX_LINE_BYTES`、`worker_response`、`rpc_response_event`）。

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| 入口与参数 | `run_worker_rpc(args)`，`--session/--model/--provider/--agent` 从 env args 解析 | [src/worker_rpc.rs:129](../../src/worker_rpc.rs#L129)，`WorkerRpcArgs::from_env_args` :76 |
| stdin 读取 task | 独立 tokio task 读行（16MB 上限）、JSON 解析、`_reply_to` 帧就地拦截（防 run 期间主循环死锁） | [src/worker_rpc.rs:1266](../../src/worker_rpc.rs#L1266)，`MAX_LINE_BYTES` (16MiB) in [ion-protocol/src/lib.rs:62](../../ion-protocol/src/lib.rs#L62) |
| 主循环与孤儿防护 | `select! { stdin_rx, orphan_tick(30s) }`；每 30s 校验 `ION_HOST_PID`，宿主死亡自退走保存路径 | [src/worker_rpc.rs:1388](../../src/worker_rpc.rs#L1388) |
| 空闲臂分发 | 163 个命令式臂（查询/消息/模型/工具/审批/扩展/MCP/编排/分支），兜底 Unknown command | `match method.as_str()` in [src/worker_rpc.rs:1424](../../src/worker_rpc.rs#L1424) |
| bg 臂（run 期间） | agent.run 嵌套 select 继续消费 stdin；只放行不碰 `&mut agent` 的只读/审批/队列类命令，其余回 busy 兜底 | [src/worker_rpc.rs:2164](../../src/worker_rpc.rs#L2164)，兜底 :2472 |
| 输出汇聚 | `output_response` / `output_error_response`——响应必带 command 回显、error 脱敏，并自动广播 `rpc_response` 摘要事件 | [src/worker_rpc.rs:7231](../../src/worker_rpc.rs#L7231) / :7236，`emit_rpc_response_event` :7437 |
| 执行域四命令 | prompt/steer/abort/followUp 在两臂各有语义（详见 Runtime path 第 4 步） | prompt :1951/:2413、steer :2596/:2371、abort :2634/:2365、follow_up :5617/:2389 |
| append 白名单 | `append_entry` 只放行 custom/custom_message/system_event/label 四种 type；落盘统一走 `append_session_entry`（leaf 解析 + 缓存失效） | `build_append_entry_data` in [src/worker_rpc.rs:7400](../../src/worker_rpc.rs#L7400)，`append_session_entry` :7723 |
| spawn 协议（host 侧） | `current_exe() + --mode rpc + --session/--model/--provider[/--agent]`，env 透传 ION_SESSION_ID / ION_FORK_CHILD / ION_HOST_PID / 桥接与限制类变量 | `worker_registry.rs:1608](../../src/worker_registry.rs#L1608)，env 收集 :1151-1176 |
| bridge 超时矩阵 | manager 通道分档超时：快命令 30s / mcp_call_tool 等 70s / create_worker 等 320s；`ION_BRIDGE_TIMEOUT_MS` 可整体覆盖 | [src/worker_rpc.rs:7572](../../src/worker_rpc.rs#L7572) |
| schema 契约 | schemas/rpc/worker/ 25 命令 + _index；静态编译 + 动态真实响应过 jsonschema 双层 | [schemas/rpc/worker/](../../schemas/rpc/worker/)，`tests/rpc_schema_worker_test.rs` |

truth 分层：命令名与参数形状以 schema（schemas/rpc/worker/）+ u16 全覆盖冒烟为准；行为语义以本页与各子系统现状文档为准；worker 拿到的环境事实（session 文件、host 存活）以注入的 env 为准。

## Runtime path

1. **Spawn.** host 构造 `current_exe() + --mode rpc` 子进程（[src/worker_registry.rs:1608](../../src/worker_registry.rs#L1608)），注入 env：`ION_SESSION_ID`（hooks common_fields 的 session 口径）、`ION_FORK_CHILD=1`（fork 子 worker 用独立 `<sid>.jsonl`）、`ION_HOST_PID`（孤儿防护）、`ION_MAX_TURNS`/`ION_ALLOWED_TOOLS` 等限制、桥接模式注入 `ION_PROVIDER_BRIDGE` 系列。Covered by `u19_session_index_updates` 家族 + `rpc_schema_worker_test.rs` 动态层.
2. **Ready.** worker 起来后 println `worker_ready_event`（:1375）——host 收到才把 Busy 解除为 Idle；ready 之前 host 视角恒 Busy（2026-08-27 gateway bootstrap busy-empty 根因）。
3. **Dispatch（空闲臂）.** 主循环对每帧 `match method`（163 臂）：查询类直读返回；`prompt` 空闲时直接 `agent.run_with_images`；`!cmd` 拦截为直执行 bash（默认 30s 超时）；未知命令回 `Unknown command`（也走汇聚点，故也是 rpc_response 事件）。
4. **Dispatch（bg 臂）.** run 期间嵌套 select 放行约 16 个命令：只读（get_messages/list_turns/get_turn_detail）、审批（review_*/ask_respond）、`llm_chunk` 桥接、abort（`stopped_handle.store(true)` + pause 复位，**不能** `agent.stop()`——借用冲突且 pause 置真会造僵尸 run）、steer/followUp（**排队**进 `pending_steer_queue`，run 结束 drain）、`prompt`（**活递**：直接 `follow_up_tx.send`，绕过 run 结束才 drain 的队列——那曾是消息黑洞）；`get_session_info` bg 版返回简化态（message_count=null，`&mut` 冲突）；其余回 busy 兜底（:2472）。
5. **Output.** 每条响应经 `output_response` / `output_error_response`（:7231/:7236）：ion-protocol 信封（command 回显 + error 脱敏截断）+ 自动广播 `rpc_response` 摘要事件（method/id/success/error 截 200 字符，不带响应体——防挤爆 EventBus bounded 1000）。Covered by `tests/rpc_event_push_ci.sh`.
6. **Exit.** 循环 break 后 `save_worker_session` + `mark_queued_inputs_consumed`（:6322-6331）；孤儿防护每 30s 查 `ION_HOST_PID`，宿主已死也走同一条保存退出路径（W8）。Covered by `test_orphan_decision_matrix`, `test_orphan_host_pid_env_gating`.

## Constraints and failure behavior

- 响应 never 直接 `output()` 绕过两个汇聚点——绕过即丢失 `rpc_response` 事件，多终端同步（webui/IDE）对该命令失明。
- 主循环 match 臂内错误路径禁止 `return`（杀死 worker 进程）——闭包收 Result 再统一 output，范本是 `fork`（:3710）与 `checkout_branch`（:3838）。Covered by 2026-08-24 事故（[LESSONS](../LESSONS.md)）。
- `append_entry` 白名单四种 type（custom/custom_message/system_event/label），拒绝一切伪造核心 entry（如 `type=message`）落盘——修复前 `_ => params.clone()` 允许伪造（W6）。Covered by `test_append_entry_rejects_message_type`, `test_append_entry_rejects_core_entry_types`.
- 同名命令两臂语义可以不同：bg 臂 `steer` 排队、bg 臂 `prompt` 活递——改执行域命令时两臂都要看，never 只改一处。
- abort 的 bg 臂 never 用 `agent.stop()` / never 置 pause=true——stopped_handle + `pause_tx.send(false)`，否则下一轮 run 挂死（僵尸 run 事故，`27bad45`）。
- JSONL 行长上限 16MiB（`MAX_LINE_BYTES`），超限发 `line_too_large` 帧后断开——读点三处统一（P0.1）。
- `_reply_to` 帧在 reader task 内就地 deliver，never 进主循环——run 期间主循环消费不到它，进主循环即死锁。
- worker_ready 事件是 host 解除 Busy 的唯一信号；缺它 host 永远视 worker 为 Busy。
- 默认预算：max_turns 20（`ION_MAX_TURNS`，0=无限）、outer iterations 5、LLM 重试 30 次/预算 240s、idle 120s、graceful drain 60s——重派路径由 [worker-supervision](./worker-supervision.md) 覆盖 200。
- 新增命令必填参数时，同步更新 u16 冒烟表（[tests/unit_rpc_test.rs](../../tests/unit_rpc_test.rs)）与 worker 域 schema——漂移在直推下无人发现（CI 直推只跑 lib 测试）。

### Constraints for fixes

改命令处理臂时：错误走闭包收 Result；响应走两个汇聚点；两臂都要改；参数校验失败用 `output_error_response` 带参数名。

## A smoke table that drifted from its commands

u16_all_supported_commands 用无参冒烟扫全命令表，断言"不返回 success:false"。`fork` 在 `aa38d53`（2026-08-25）从存根变真实现后要求 `turnId`，无参调用必失败——但该测试红了一个半月无人看见：ci.yml 直推 master 只跑 `cargo test --lib`，`cargo test --tests` 仅 pr-gate 跑。Before, 修复方式有两种争议（补 turnId 造真实 turn vs 移出冒烟），现状取后者：fork 移出无参表 + 专项断言验分发与参数校验存活，完整行为归 `tests/branch_tree_ci.sh`（真实 turn 13/13）。教训入册 [LESSONS](../LESSONS.md)（改命令签名必须同步 u16）。Covered by `u16_all_supported_commands`, `tests/branch_tree_ci.sh`.

## Verification

```sh
cargo test --test unit_rpc_test
cargo test --lib -- test_append_entry test_orphan test_abort_bash
cargo test --test rpc_schema_worker_test
bash tests/rpc_event_push_ci.sh
```

本次快照（基准 98e02ca 后加 u16 修复，2026-10-14）实测：`unit_rpc_test` **20/20**（修复前 19/20，fork 漂移见专题节）、lib 过滤集 12/12、`rpc_schema_worker_test` **5/5**（静态 4 + 动态 1：隔离 host 真实响应过 jsonschema）。环境前提：unit_rpc_test 自带 WorkerProc 子进程 harness（spawn 真实 worker，不碰真实 `~/.ion`）；rpc_schema_worker_test 起隔离 host（私有 HOME + `ION_HOST_SOCKET`）；`rpc_event_push_ci.sh`（rpc_response 推送端到端）本次 not run。全量 lib 回归按仓库惯例 `--test-threads=2`。
