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

**读进程 env 的单测，env_test_lock 只解决"测试互串"不解决"真实 shell 变量渗入"——必须配 RAII 摘除恢复，且同坑多测试一次补全。**
- Before, X2 flake 审计给 remote_worker 配置解析测试统一上了 `paths::env_test_lock`（只串行化、不移除变量），但开发/CI 脚本环境常态 `export ION_REMOTE_WORKERS`（remote_worker_ci.sh:52 等 5 处脚本），该 env 会被 `from_config` 合并进池污染断言；且同批只给 `from_config_parses` 加了摘除、`profile_reads_config_fields` 漏网——同一个坑两处只补一处。
- 证据：本 commit（config.rs `RwEnvGuard` + sandbox_pool.rs `defer` RAII 三处，补漏网测试；复现 `ION_REMOTE_WORKERS='{"ghost":{"hostname":"x"}}' cargo test --lib -- profile_reads_config_fields`，修前 ghost host 会渗入池）；同家族 `4604b12`（ION_SESSION_CWD）、`72cd162`（X2 HOME env 竞态 RAII 兜底）。
- Seen 1× (2026-10-15)

**守卫/校验脚本的"本地绿"不算数——凡依赖开发机才存在的路径（兄弟仓库、gitignored 目录）在 CI checkout 必红，排除规则要建在链接形状层而不是靠本地碰巧存在。**
- Before, docs_links_ci.sh 排除规则只有协议/占位符类，AGENTS.md 链 `../ion-webui/`（兄弟仓库）与 HOOKS_ION_KILL 链 `~/.zcode/`（gitignored 目录）本地全绿，CI docs-links 三连红无人见。
- 证据：本 commit（脚本增 `escapes_repo` 排除 + 环境专属链接改内联码）；CI 失败记录 gh run 38577387691（[FAIL] 三条）。
- Seen 1× (2026-10-15)

**跨平台测试不得把大缓冲塞进单个 argv——Linux MAX_ARG_STRLEN 128KB/参数、macOS 宽松，本地绿 CI（Linux）红；大输入走 stdin 或临时文件。**
- Before, `write_file_commands_contract` 把 267KB 的 base64 拼接串作单个参数传 `sh -c`（macOS 过），CI 报 `ArgumentListTooLong`（1195/1196 唯一红）；生产侧 `WRITE_FILE_CHUNK_B64=32KB` 分块本身安全，纯测试 bug。
- 证据：CI run 38681215820；修复 = 改 stdin 喂 `base64 -d`（本 commit，验证语义不变）；与同日两条（链接守卫路径、工具链漂移）构成"本地绿≠CI 绿"三件族。
- Seen 1× (2026-10-15)

**CI matrix runner 的 per-script work dir 是手工拼的符号链接仓库——新增 workspace 成员 crate / 根级构建文件（build.rs）时必须同步链接清单，漏了 = 每脚本 Phase 0 真编译秒败且被 runner 的输出过滤掩盖。**
- Before, run_ci_matrix_parallel.sh（42df79d，2026-09-09）的 work dir 链接清单缺 `ion-protocol/`（9-15 新增 crate）与 `build.rs`——绕过 cargo shim 走绝对路径 `~/.cargo/bin/cargo` 的脚本（如 approval_bus_ci:191）在 work dir 真编译即失败；CI run 38684860314 ci-matrix 47/91 连败全在 0-2s，报错行被 runner 的 `grep -v "command not found"` 过滤吞掉，只能靠 artifact 日志（`FAIL build ion`）定位。八月底后新写脚本从未被 matrix 验证过（build-and-test 红六周 → ci-matrix 恒 skipped）。
- 证据：本 commit（runner 补 `ion-protocol` + `build.rs` 链接，清单处加"新增成员必须同步"警告注释）；本地模拟 work dir 复现三态（缺 ion-protocol 秒败 / 缺 build.rs 报 env!未定义 / 双补后 3m52s 构建成功）。
- Seen 1× (2026-10-15)

**改命令签名（新增必填参数）必须同步 u16 全覆盖冒烟表——无参冒烟表与签名漂移会红得无声（直推只跑 lib 测试，集成测试仅 PR 才跑）。**
- Before, `fork` 自 `aa38d53`（2026-08-25）变真实现要求 turnId，`u16_all_supported_commands` 无参表红了一个半月无人见——ci.yml 直推 master 只跑 `cargo test --lib`，`cargo test --tests` 仅 pr-gate 跑，直推绕过了它。
- 证据：u16 失败复现（tests/unit_rpc_test.rs:693 报 "fork: missing param: turnId"，2026-10-14 发现）；修复 = fork 移出无参冒烟 + 专项断言验分发与校验（完整行为归 branch_tree_ci）；缺口对照 `.github/workflows/ci.yml` vs `pr-gate.yml`（`cargo test --tests` 仅后者有；同日 ci.yml 增 integration-tests job 补位）。
- Seen 1× (2026-10-14)

**CI 门禁用浮动 stable 而开发机版本滞后时，"本地全绿"与 CI 会长期漂移——修红前先用与 CI 同版工具链复现，修完双版验证；上游宏引发的 lint 误报收口到 workspace 级豁免而非散布 allow。**
- Before, ci.yml 用 `dtolnay/rust-toolchain@stable`（已浮到 1.99，double_must_use 对 async_trait 0.1.89 展开全线误报），本地默认 1.97 看不到该错误、只看得到另外 12 个旧 lint；build-and-test 自 2026-08-31（01f2b37）后连红六周无人见——每次直推都被 clippy 挡在 fmt/test 步骤之前，红被误读为"CI 问题"而非"门禁在报存量"。
- 证据：CI run 38577387691 连红记录；本地 `cargo +1.99.0 clippy` 复现同错；修复 = `[workspace.lints.clippy]` 一处豁免 double_must_use + 12 个 lint 清零 + 全仓 fmt（本 commit）；`cargo fmt --check` 双版（1.97/1.99）验证一致通过。
- Seen 1× (2026-10-15)

**"持有外层 registry 锁跨 .await 调会回锁同一互斥量的方法"是确定性死锁模式——测试与生产代码同罪；一个挂死测试会拖死整个二进制与 CI 门禁，发现即 ignore + 取证留痕，不许带病入库。**
- Before, manager_integration（i06/i15/i17/i24/i31）与 e2e_stress（e01b/s01/s04）共 8 个测试持 `registry.lock()` 守卫跨 `.await` 做事件/channel/create 操作，采样栈卡在 `WorkerRegistry` 内 parking_lot `lock_slow`；空闲环境逐测试超时扫描逐一确认（并发扫描会出假阳性——bash02/e01/e04 三例并发挂、单跑过，复验必须在空闲环境做）。该家族从未被发现，因为集成套件在 bde4655 加 CI 门禁前从未全量跑过（正是上一条 u16 教训的延续）；并发跑时挂死测试还会连坐同批测试（i30 曾被误判）。
- 证据：本 commit（8 处 `#[ignore]` 带证据注释 + 保留用例）；复现 `timeout 25 target/debug/deps/manager_integration-<hash> --exact i06_worker_events_forwarded --test-threads=1`（EXIT=124，其余同法）；采样栈 `sample <pid>` 显示测试线程 lock_slow、worker 子进程 idle parked；既有先例 `#[ignore = "drain_events disabled..."]`（manager_integration L572，drain_events 架构改造时只处理了一个）；修复方向=channel/drain/create 路径与外层锁的持锁顺序审计（参考 7b4dfc4 lock-split）。
- Seen 1× (2026-10-15)

**静态化/锁拆分重构时核对被绕开的实例状态——静态路径重建的解析优先级会与实例路径分家（worker_bin vs find_ion_binary），测试进程里 current_exe 不是 ion。**
- Before, `76121de`（2026-08-09）把委托 create_worker 切到静态 `prepare_worker_spawn` 后，其 `find_ion_binary` current_exe 优先、拿不到 `with_binary` 注入的 worker_bin——测试进程里 current_exe 是测试二进制（libtest 不认 `--mode`），委托/respawn/host 静态路径 spawn 的子 worker 秒死 exit=101 "Unrecognized option: 'mode'"，e01 红了两个月无人见（与直推不跑集成测试叠加）。
- 证据：诊断实证（reviewer exit_reason=测试二进制的 libtest 措辞，单独运行测试二进制复现同错）；修复 = `WORKER_BIN_OVERRIDE` 进程级（with_binary 设置 / find_ion_binary 最优先），e2e_stress 18/18 复绿；生产不受影响（serve 不调 with_binary）。
- Seen 1× (2026-10-14)

**集成测试夹具必须 hermetic：HOME 隔离 + 信任门显式放行 + session_id 全链对齐——产品加固后夹具不跟就静默红（直推不跑集成测试年代无人见）。**
- Before, 两个家族各漏一源：hooks_e2e 读真实 HOME 的 `~/.ion/hooks.json`（zcode 防误杀 hook 泄漏进 event_count，2 vs 4 按机器漂移）且被 M4 供应链信任门拦掉项目级加载；file_snapshot_harness 的 `new_pair_with_cwd` 硬编码 session_id="test" ≠ ApprovalManager 的会话 id，de08097 去掉全局回退后 baseline 永远缺失。
- 证据：hooks 修复 = `hermetic_load_fresh`（隔离 HOME + `load_fresh_with_trust`，10/10）；快照修复 = 显式 `new_pair` 同 id + h7 断言对齐"reject 保留锚点"语义（10/10）。
- Seen 1× (2026-10-14)

**公开 API 的参数不许在存储迁移时无声变 no-op——保留签名丢语义比删参数更糟（调用方以为过滤了其实没有）。**
- Before, memory `search_with_scope` 的 outline 参数在 V0.2 FTS 迁移后变 `_outline` 无声忽略（FTS 无 outline 维度），v0.1 fallback 路径也跟着丢了过滤；memory_e2e 的 outline 过滤断言红（"search rust in tech should find 0" 实得 1）。
- 证据：修复 = fallback 路径恢复按 outline id 过滤 + FTS 路径注释显式声明"该参数被忽略"；memory_e2e 6/6、lib memory 26/26。
- Seen 1× (2026-10-14)

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
