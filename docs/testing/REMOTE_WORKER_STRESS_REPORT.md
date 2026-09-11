# ION 远程执行压力测试 — 问题分类报告

日期：2026-09-11 ~ 09-12 | 环境：Mac (M1) + win38 (Win10/WSL2) | 模型：GLM-5.2

## 分类标准
- **工程问题（ION 的锅）**：ION 内核/架构的 bug 或设计缺陷
- **模型问题（GLM 的锅）**：LLM 的行为/推理质量
- **配置问题**：默认值不合理，调一下就好
- **操作失误**：人的问题

---

## 问题清单（按发现顺序）

### P1: worker 干着干着就停
**分类：配置问题（不是 bug）**
- 现象：Mission B 主 worker 19 轮 + 30 次工具调用后停止
- 根因：`ION_MAX_TURNS` 默认 20（worker_rpc.rs:447 `.unwrap_or(Some(20))`）
- 证据：assistant 消息数 = 19 + 初始 = 正好 20 触顶
- 修法：agent .md 加 `maxTurns: 100`（已修 ✅）

### P2: worker 意外死亡（SSH 断连）
**分类：工程问题（ION 架构约束）+ 操作失误**
- 现象：远程 worker 被 SSH 连接断开杀死（发生 3 次）
- 根因：远程 worker 生命周期绑定在 Mac host 进程的 ssh 子进程上
- 加重因素：调试期间反复杀 host（操作失误）导致 ssh 大量断连
- 已缓解：ServerAliveCountMax=10（~5 分钟瞬断容忍）
- 根治方向：远程 worker 改为 serve 的子进程而非 ssh 的子进程（C 模式）

### P3: Manager 死锁（host 无响应）
**分类：工程问题（ION 的 bug）**
- 现象：Mac host socket 活着但不处理任何 RPC
- 栈取样：主 serve 循环 + register_prepared_workers 都卡在 parking_lot lock_slow
- 根因：bridge_serve_llm_request 的 spawn 任务在流式转发 LLM chunk 时
  `registry_arc.lock() → write_line_to_worker_sync → thread::sleep(2ms) 重试`
  阻塞 tokio worker 线程持锁，多个桥接任务轮流抢锁造成活锁
- 状态：待修（需要 write 改真 async 或锁粒度细化）

### P4: 子 Worker 初始 prompt 注入丢失（两轮 Mission D 均复现）
**分类：工程问题（ION 的 bug）**
- 现象：子 Worker 进程在 win38 落地但 0 工具调用（会话文件只有 header）
- 复现率：100%（2/2 轮次）
- 根因：coordinator（远端）→ manager_command → Mac Manager spawn 子 worker → 
  prompt 注入依赖 worker_ready 事件 → 子 worker 的 ready 事件未到达/未被处理
- 与 P2/P3 的关联：在 host 混乱期和死锁期都发生过，但干净环境下第二轮仍复现
- 状态：待修（多级 spawn 的 prompt 注入链路不可靠）

### P5: 子 Worker create_worker 超时（320s）
**分类：工程问题（与 P3 同根因）**
- 现象：coordinator 的第二个 spawn_worker 请求超时
- 根因：process_pending_commands 持锁做慢操作（refresh + assets deploy + ssh spawn），
  同时桥接任务也在抢锁 → 超时
- 与 P3 关联：都是锁竞争问题

### P6: LLM 思考期空闲超时误杀
**分类：工程问题（已修 ✅）**
- 现象：GLM-5.2 推理首 token 超 2 分钟被 worker 判定"上游挂死"
- 根因：agent_loop 空闲超时 120s 硬编码
- 修法：ION_LLM_IDLE_TIMEOUT_MS 可配，桥接 worker 注入 600s

### P7: npm 网络问题（r.cnpmjs.org 死镜像）
**分类：环境问题（非 ION 非 LLM）**
- 现象：npm create 打到已死镜像 ETIMEDOUT
- 修法：export npm_config_registry=npmmirror（已写进任务提示词）

---

## 模型（GLM-5.2）的表现评估

| 维度 | 评分 | 说明 |
|------|------|------|
| 任务理解 | ✅ 好 | 正确理解 Mission B/D 的复杂指令 |
| 编排决策 | ✅ 好 | 自主选择 developer 角色、正确传递 host 参数、任务书含环境提示 |
| 代码质量 | ✅ 好 | module-inventory 结构跟随模板模式、price 用分存储避免浮点、测试覆盖 CRUD |
| 排障能力 | ✅ 好 | 自主诊断 npm 镜像问题、tsc 类型错误定位准确、vitest 失败原因分析到位 |
| 诚实度 | ✅ 好 | DEV-NOTES 如实报告"工具调用额度耗尽"、不谎报完成 |
| 弱点 | ⚠️ | 长任务中途停止后不会主动说"我被中断了"——这是工程侧的责任（turn 限制），不是模型问题 |

## 总结

| 类别 | 数量 | 说明 |
|------|------|------|
| 工程问题（ION） | 4 个 | P3 死锁、P4 prompt 丢失、P5 超时、P6 空闲超时（已修） |
| 配置问题 | 1 个 | P1 turn 预算（已修） |
| 环境问题 | 1 个 | P7 npm 镜像 |
| 操作失误 | 1 项 | 调试时反复杀 host 加剧了 P2 |
| 模型问题 | 0 个 | GLM-5.2 在本次测试中无行为缺陷 |
