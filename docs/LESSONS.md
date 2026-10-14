<!-- reviewed-through: ffb6d5c (2026-10-14) -->
# Lessons from merged work（错题本）

ION 的错题本：复盘**已合并/已修复**的工作——返工的、不该那样写的、验证太薄的，都收进来。与 [AGENTS.md](../AGENTS.md)（评审强制项）构成"复盘发现 → 错题本（带证据）→ 三振转正 → 评审强制项"的闭环。

写法红线：

- 规则**永远一句祈使句加粗**；证据**永远具体到 commit SHA / 测试名 / AGENTS.md 修复批（日期）**——没有证据的教训不存在。
- "Before, <旧行为与事故>" 保留 why；有做对的正例就补 `Done right on <日期>: <SHA>`。
- `Seen N×` 数的是**复盘发现它的天数**（不是出现次数）。同一条教训 ≥3 个不同日期 → 搬进 [AGENTS.md](../AGENTS.md) 对应章节（三振转正），本文件只留指针。
- 复盘是判断型工作，**手动启动**：每完成一个修复批/特性后，从书签 `reviewed-through` 接着读增量 commit；不做 cron。

复盘记录：2026-10-14 首跑——深挖 2026-08-15..ffb6d5c 共 286 个 commit 的返工信号词（回归/复发/撤销/死分支/恒 0/假成功/吞/漏传），新增 5 条（见各节 2026-09 日期条目）。

---

## 修复要断根

**改取消/暂停类共享通道前，先 grep 全库确认语义闭环——有置位必有复位，有 send 必有消费。**
- Before, `Agent::stop()` 的取消信号错用 pause watch channel 且全库无 resume 调用者，pause=true 永久残留，abort 后同 worker 下一轮 run 永久挂起（真实环境复现 2 次，≥300s 不恢复）。
- 证据：`27bad45`（AGENTS.md 2026-09-16 修复批 6①；harness `abort_hang_harness`）。
- Seen 1× (2026-09-16)

**重构提交后，盯默认分支的参数语义回归——谁把谁当 id。**
- Before, 4dd79da（2026-08-10 parking_lot 重构）之后 `handle_manager_command_write` 默认分支把 session_id 当 worker_id 传 send_async，prompt 撞 worker 重建窗口被吞（返回 success 但新 worker 永不执行）。
- 证据：`ea52dfb` 修 4dd79da 引入（harness `prompt_rebuild_window_harness` 7 点位采样，修前 4/4 吞没→修后 7/7 投递）。
- Seen 1× (2026-09-16)

**修"漏传参数"类 bug，全仓 grep 同类构造点一次补全——同一种配置点往往不止一处。**
- Before, AUTO-RECOVERY respawn 只补了 worker_registry stdout-EOF 一处，bin/ion.rs heartbeat 判死路径仍漏传 max_turns，重派 worker 吃默认 20 turns 烧尽。
- 证据：`82fd407`，提交信息原文："bin/ion.rs heartbeat 重派路径补 max_turns:200（与 worker_registry stdout-EOF 路径一致，此前漏传吃默认 20）"。
- Seen 1× (2026-09-14)

## 跨进程与 Worker 纪律

**任何"整条替换"式写入（upsert/insert 全量换）之前，重建路径必须从 existing 带回旧值。**
- Before, SessionIndex upsert 是 `sessions.insert` 全量替换，register 复活不带旧值，标题/计数器/project 归属被清空（消息落错项目目录）。
- 证据：`de08097`（upsert 三类兜底 + `merge_existing_meta`）；现状契约见 [subsystems/session-index.md](./subsystems/session-index.md)。
- Seen 1× (2026-08-25)

**worker 主循环 match 臂内的错误路径禁止 `return`——用闭包收 Result 再统一 `output_error_response`。**
- Before, match 臂内 return 直接杀死 worker 进程，整个会话失联。
- 证据：`aa38d53`（分叉树闭环同期，AGENTS.md「分叉树闭环」条目原文，2026-08-24 实测）。
- Seen 1× (2026-08-24)

**跨进程排队/缓冲一律落盘（append-only + 消费标记 + 回放幂等三闸）——纯内存队列在 kill -9 下是黑洞，排队回执 ≠ 送达。**
- Before, steering/followUp/next_turn 三队列 + pending_steer_queue + follow_up_tx 全内存，crash 时排队消息凭空蒸发。
- 证据：`2dc34d5`（custom/queued_input 协议；`tests/queue_persist_harness.rs` T1-T5）；与 respawn 的衔接见 [subsystems/worker-supervision.md](./subsystems/worker-supervision.md)。
- Seen 1× (2026-09-15)

## 注入与转义

**跨边界传 payload（远端/容器/ssh）弃 heredoc 与字符串拼接，改 base64 / argv 分块。**
- Before, write_file 用 heredoc 转运，IONEOF 注入可 RCE；资产同步 tar 拼文件名同样可注入——同一天两处独立爆雷。
- 证据：`e6da22c`（W5 write_file base64 转运）、`4c45346`（W8 资产同步 tar argv 化）。
- Seen 1× (2026-09-15)

## 验证与测试

**测试基建按真实平台约束设计（路径长度/临时目录/并发）——测试挂了先怀疑测试自身，再怀疑被测物。**
- Before, 特征测试用长前缀+纳秒随机拼 socket 路径，超 macOS SUN_LEN 104 字节致 host bind 失败，被误报为被测物问题。
- 证据：`bee7f80`。
- Seen 1× (2026-09-15)

**CI 步骤禁止 continue-on-error / `|| echo` 吞错——退出码必须向上传播，断言失败就是失败。**
- Before, ci.yml 多处 continue-on-error + 集成测试被 `|| echo` 吞掉，存在伪造 "900 passed" 的 shim，坏提交照样绿。
- 证据：`42df79d`（T03；`tests/ci_trust_gates_ci.sh` 21 断言故障注入）。
- Seen 1× (2026-09-09)

**cargo 测试过滤器是子串匹配，不是正则——多测试名用空格分隔传，`|` 串 0 命中是假成功。**
- Before, `-- 'a|b|c'` 报 "0 passed; 1196 filtered out"，退出码 0 看似 ok 实则一个没跑。
- 证据：[subsystems/session-index.md](./subsystems/session-index.md) §Verification（2026-10-14 实测；空格分隔过滤 8/8 对照）。
- Seen 1× (2026-10-14)

**统计口径上线前，先在目标模式实测非退化——某模式下恒 0/恒空的指标在该模式不可用，别等用户报数才发现。**
- Before, hooks token 用 messages 增量口径，llm_bridge 模式下恒 0（远端桥接不走 messages 计数），换 SessionIndex 会话累计才对本地/远端都准；同家族：hooks common_fields 的 session_id 此前恒空（存量缺口）。
- 证据：`3182659`（口径切换，提交信息自带"已实证"）；`5509206`（恒空存量缺口）。
- Seen 1× (2026-09-23)

**新事件/转发出口必须用 subscribe 端到端实测通电——"代码里有转发分支"≠"事件真的到得了"（死分支条件永假，编译单测全绿）。**
- Before, serve pump 对 `mtype=="extension_event"` 的查询条件永假，worker 审批事件从未进 EventBus，修复前任何测试都是绿的。
- 证据：`3aebd19`（修复）+ `e0934be`（补 subscribe 实测验证；AGENTS.md 2026-09-16 修复批 7 附带发现）。
- Seen 1× (2026-09-17)

**停止/清理路径先验证目标身份再动文件——"假成功"比失败危险。**
- Before, `ion serve stop` 无 shutdown 处理者：host 永远停不掉，还无条件盲删 sock/pid 文件（拆活 host 的入口文件 = DoS 面）；重写为确认式退出（发 shutdown → 轮询 socket 消失 ≤8s → 确认才报成功，超时走 pid 身份复核强杀）。
- 证据：`281560e`（G1 冒烟暴露的既有 bug）。
- Seen 1× (2026-09-16)

**新增全局 env 透传前先评估并行测试串扰——lib 测试共享进程 env 会互相污染；不可隔离就参数化或放弃该 env。**
- Before, worker spawn 新加 `ION_SESSION_CWD` 透传（治 hooks session_id 恒空），同日即因测试并发污染撤销，只保留 ION_SESSION_ID。
- 证据：`4604b12` 撤 `5509206`（同日加撤对，lib 1196/0 验证）。
- Seen 1× (2026-09-23)

## 文档与证据

**引用 commit SHA / 测试名 / 文档名之前，先在正确的仓库 `git log` / grep 验伪——凭记忆引证必错。**
- Before, 2026-10-14 建立本文件时凭记忆引用的 `f47cf21`、`7a2c2b9` 在本仓 master `git log` 均 NOT FOUND（后者实为 ion-webui 另一个仓库的 SHA；前者已从 session-index.md 删除）。
- 证据：本文件建立时的验伪记录（2026-10-14）；同类红线见 [templates/SUBSYSTEM_DOC_TEMPLATE.md](./templates/SUBSYSTEM_DOC_TEMPLATE.md) §证据锚点。
- Seen 1× (2026-10-14)

**批量改文档用带锚点的精确编辑，不用 sed 裸模式——BRE 里 `.` 是通配符，含点模式会误伤链接文字。**
- Before, 2026-10-14 修坏链时 `sed 's|../design/MEMORY_EXTENSION.md|...|'` 把 TEST_SPEC_TEMPLATE.md 标签里的 `docs/design/...`（`cs/design` 段被 `.` 通配命中）替换成残文 `do../archive/...`，git diff 复查时抓获。
- Done right on 2026-10-14: 其余 12 处链接修复后立即跑全仓链接扫描复验。
- Seen 1× (2026-10-14)

---

## Moved to the code standards

（空——尚无三振条目。第一条产生时：规则原文 + 三个日期 + 证据摘要搬进 [AGENTS.md](../AGENTS.md) 对应章节，此处只留一行指针。）
