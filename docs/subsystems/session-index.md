# Session index: 会话热字段目录与跨进程读写协议

> **状态：现状快照** — 基准 commit ffb6d5c（2026-10-14，验证情况见 [Verification](#verification)）。

SessionIndex 是所有会话的摘要索引，物理上是单个 JSON 文件 `~/.ion/agent/sessions.index.json`（`SessionIndex::path`，[src/session_index.rs:199](../../src/session_index.rs#L199)）：每个会话一行 `SessionMeta`——标题、血缘、model/provider、token/turn 统计、worktree 状态。host 进程与所有 worker 进程都直接读写它，没有单一属主。

本页讲索引本身的契约：字段怎么进、跨进程读写协议（flock、原子写、墓碑、损坏处理、启动对账）和消费方。会话正文的 JSONL 条目格式在 [SESSION_MESSAGE](../design/SESSION_MESSAGE.md)，会话文件布局与 GC 在 [SESSION_ISOLATION](../design/SESSION_ISOLATION.md)，"什么字段该进索引"的裁决标准在 [AGENTS.md §存储落位原则](../../AGENTS.md)——设计动机不再重复。

## Responsibilities and sources of truth

| Part | Responsibility | Source |
| --- | --- | --- |
| 字段集 | 一行会话摘要的全部字段。新字段必须小且可枚举，带 `#[serde(default, skip_serializing_if = "Option::is_none")]`；老索引文件靠 default 兜底 | `SessionMeta` in [src/session_index.rs:8](../../src/session_index.rs#L8) |
| 路径与锁 | 索引文件与其 `.json.lock` 锁文件的位置（`$HOME` 回落 `USERPROFILE` 再回落 `.`） | `path` / `lock_path` in [src/session_index.rs:199](../../src/session_index.rs#L199) |
| 跨进程写事务 | flock 独占锁内 load→mutate→save 的统一写入口；写事务内墓碑超 512 全清 | `write_txn` in [src/session_index.rs:227](../../src/session_index.rs#L227) |
| 损坏处理 | 读侧撞损坏返回空视图并记录；写侧撞损坏先隔离改名再从空重建 | `load` / `load_for_write` / `quarantine` in [src/session_index.rs:266](../../src/session_index.rs#L266) |
| 原子落盘 | `.json.tmp` 写入后 rename；失败返回 false 并记录 `SaveFailed` | `save` in [src/session_index.rs:329](../../src/session_index.rs#L329) |
| 整条替换 | upsert 是 `sessions.insert` 全量替换，仅三类兜底（见 Constraints） | `upsert` in [src/session_index.rs:425](../../src/session_index.rs#L425) |
| 构造 | 从 existing meta 带回历史字段（计数器、created_at、标题）后落新条目 | `build` / `update` in [src/session_index.rs:452](../../src/session_index.rs#L452) |
| 部分更新族 | 14 个 `set_*` / `increment_*` 方法 + 通用 `patch_meta` 漏斗（缺条目先建最小条目）；`update_workspace` 是 host 侧部分更新范本 | `patch_meta` in [src/session_index.rs:616](../../src/session_index.rs#L616)，`set_model` in [src/session_index.rs:717](../../src/session_index.rs#L717) |
| 墓碑 | `removed_sessions` 集合：删除落墓碑、复活清墓碑、守卫拒绝已删会话的补写 | `remove_persist` / `is_removed` in [src/session_index.rs:382](../../src/session_index.rs#L382) |
| 启动对账 | host 启动时扫磁盘 JSONL，为索引缺失的会话从 header 重建条目 | `reconcile_missing` in [src/session_gc.rs:87](../../src/session_gc.rs#L87) |
| 复活合并 | create_session 路径调 upsert 前把旧 meta 的历史热字段带回新 meta | `merge_existing_meta` in [src/worker_registry.rs:10](../../src/worker_registry.rs#L10) |
| 空闲合成读 | 无 worker 时 host 从索引合成 `get_session_info` / `get_context_usage` / `get_active_tools` 响应，不拉起 worker | `host_idle_session_read` in [src/bin/ion.rs:8216](../../src/bin/ion.rs#L8216) |
| 全量列表 | `list_all_sessions` 在写事务内读全索引并懒修复 message_count | [src/bin/ion.rs:8545](../../src/bin/ion.rs#L8545) |
| 索引健康 | `get_index_health` RPC 暴露 `last_issue`（LockDegraded / SaveFailed / QuarantinedCorrupt 三分类） | [src/bin/ion.rs:8606](../../src/bin/ion.rs#L8606) |
| 删除入口 | `session_remove` RPC → `remove_persist`（删条目 + 插墓碑） | [src/bin/ion.rs:8772](../../src/bin/ion.rs#L8772) |

正文与摘要的 truth 分层：会话 JSONL 是更底层的真相——索引条目可由 JSONL 对账重建（`reconcile_missing`），JSONL 丢了索引无能为力。"哪些会话存在 + 热字段" 以索引为直接数据源（UI 列表、刷新恢复直读它）。

## Runtime path

1. **Boot.** host serve 启动时 `reconcile_missing`（[src/bin/ion.rs:6326](../../src/bin/ion.rs#L6326) → [src/session_gc.rs:87](../../src/session_gc.rs#L87)）扫 sessions 目录全部 `*.jsonl`，对索引缺失的会话从 header 行重建最小 `SessionMeta`（`meta_from_header`）并补 message/turn 统计，最后进写事务落盘。三纪律：只补缺不删多、跳过墓碑、零差异不进写事务。Covered by `reconcile_rebuilds_missing_index_entries_from_disk`.
2. **Create.** create_session 两条路径（[src/worker_registry.rs:1936](../../src/worker_registry.rs#L1936) 与 [src/worker_registry.rs:2776](../../src/worker_registry.rs#L2776)）在 `write_txn` 内先清墓碑，再 `merge_existing_meta` 把旧 meta 的历史热字段（计数器、标题、project 归属、血缘）带回，最后 `upsert` 落条目。
3. **Update.** 随生命周期事件顺便写入，没有后台同步：worker 侧 RPC 与 agent 循环用部分更新方法走 `patch_meta` 漏斗（`set_model`、`increment_turn_stats` 等，调用面在 [src/worker_rpc.rs:1601](../../src/worker_rpc.rs#L1601) 起的多个 handler 与 [src/agent/agent_loop.rs:1045](../../src/agent/agent_loop.rs#L1045)）；host 侧 workspace 生命周期走 `update_workspace`。缺条目时 `patch_meta` 先建最小条目再改。
4. **Read.** 三个形态：活跃会话 RPC 转发给 worker；空闲会话由 `host_idle_session_read`（[src/bin/ion.rs:8216](../../src/bin/ion.rs#L8216)）从索引合成响应，零拉起；`list_all_sessions`（[src/bin/ion.rs:8545](../../src/bin/ion.rs#L8545)）读全索引。例外：`list_sessions` 读 registry 在册 worker，**不读索引**（[src/bin/ion.rs:8530](../../src/bin/ion.rs#L8530)）。
5. **Remove.** `session_remove`（[src/bin/ion.rs:8772](../../src/bin/ion.rs#L8772)）→ `remove_persist`（[src/session_index.rs:382](../../src/session_index.rs#L382)）在写事务内删条目并插墓碑。此后 `patch_meta` 对该 id 直接 return（[src/session_index.rs:622](../../src/session_index.rs#L622)），只有显式 create/update 清墓碑（[src/session_index.rs:578](../../src/session_index.rs#L578)）。
6. **Degrade.** 索引故障从不阻塞会话功能：写侧撞损坏文件先 `quarantine`（rename 到 `*.corrupt-<秒>-<pid>`）再从空重建；隔离失败则放弃本次写。读侧撞损坏返回空视图并记录 `QuarantinedCorrupt`，`get_index_health` 可查。Covered by `index_persist_faults_are_visible_and_safe`.

## Constraints and failure behavior

- 所有写走 `write_txn` 的 flock 独占锁（[src/session_index.rs:227](../../src/session_index.rs#L227)）。写者分布在 host 与各 worker 进程，绕过事务的独立 load→modify→save 是 last-write-wins 互踩（模块注释 :219-226 即为此而写）。
- `upsert` 是整条 `SessionMeta` 替换（`sessions.insert`，[src/session_index.rs:448](../../src/session_index.rs#L448)），仅三类兜底：有意义标题（≠裸 sid）不被覆盖、11 个计数器取 max、`created_at` 取 min（:427-446）。任何重建 meta 的路径必须从 existing 带回旧值——`build()` 内置，create 路径走 `merge_existing_meta`；不带back的字段会被清空。Covered by `test_respawn_inherit_fields_takes_record_agent_and_index_model`.
- 墓碑语义：`removed_sessions` 里的 id 意味"已删除"，never "未知"。`patch_meta` 对墓碑条目静默 return；显式 create/update 才清墓碑。写事务内墓碑超 512 (512) 全清（:243）。
- 读失败 ≠ 空数据：读路径撞损坏文件返回的是**空视图 + 隔离记录**（`QuarantinedCorrupt` 经 `get_index_health` 可见），原文件已被改名保全——"列表为空"意味着"索引被隔离了"，never "会话没了"。
- 写路径撞损坏：quarantine 后从空索引重建（接受丢摘要——正文在 JSONL，可由对账追回）；隔离本身失败则 `may_save=false` 放弃本次写。宁可不写，never 用空索引覆盖现场。Covered by `session_index_health_ci.sh`（H1/H2）。
- `save` 是原子写（`.json.tmp` → rename，:341-348），失败返回 false 并记录 `SaveFailed`。返回值语义是"这次没落盘"，不是异常。
- 对账幂等三纪律（[src/session_gc.rs:16-31](../../src/session_gc.rs#L16)）：只补缺不删多、尊重墓碑、零差异不进写事务。
- 索引是目录不是仓库：字段必须小且可枚举（string/数字/bool）；大对象、列表、可重放轨迹进会话 JSONL。裁决标准是 [AGENTS.md §存储落位原则](../../AGENTS.md)，两者都不放的就进内存，宁可丢也不建新文件。
- 新字段四步，缺一步编译器会拦：`SessionMeta` 加 `serde(default, skip_serializing_if)` 字段 → 补所有构造点（`SessionMeta` **无** `Default`；构造点在 `build`、`patch_meta` 最小条目、`merge_existing_meta` 调用侧、`meta_from_header`、各测试的 `make_meta`，E0063 会逐个报）→ 在生命周期事件处顺便写初值 → 复查所有"重建 meta"路径从 existing 带回。

## A session that outlived its index entry

磁盘上有会话 JSONL、索引里没有条目——该会话从 UI 列表消失，成为"幽灵"。来源：索引文件损坏被隔离后从空重建、或历史写入失败。

Boot 步骤的 `reconcile_missing` 兜底：文件名差异集 + header-only 重建 + 统计补齐，下次 host 启动时幽灵自动回表。Before, 索引落后磁盘无自动修复，只能手工重建（AGENTS.md 2026-09-15 修复批 4 记录——"索引逆向对账"一项，治"索引落后会话成幽灵"）。Covered by `reconcile_rebuilds_missing_index_entries_from_disk`（`tests/session_reconcile_ci.rs`）.

## Verification

```sh
cargo test --lib -- test_get_children test_has_children test_child_count \
  test_root_session_has_no_parent test_forked_session_has_parent \
  test_count_sessions_by_project test_respawn_inherit
cargo test --test session_index_fault_ci
cargo test --test session_reconcile_ci
bash tests/session_index_health_ci.sh
bash tests/host_read_ci.sh
bash tests/sessions_ci.sh
bash tests/rpc_schema_session_ci.sh
```

本次快照（commit ffb6d5c，2026-10-14）实测：lib 过滤 8/8、`session_index_fault_ci` 1/1、`session_reconcile_ci` 1/1、`session_index_health_ci.sh` 5/5；`host_read_ci.sh`、`sessions_ci.sh`、`rpc_schema_session_ci.sh` **not run**。环境前提：`sessions_ci.sh` 直接操作真实 `$HOME/.ion/agent/sessions.index.json`（脚本内 cp 备份到 `/tmp` 再还原）；`session_index_health_ci.sh` 使用私有沙箱 HOME。全量 lib 回归按仓库惯例加 `--test-threads=2`。cargo 测试过滤器是子串匹配不是正则——多测试名用空格分隔传，`|` 串会 0 命中。
