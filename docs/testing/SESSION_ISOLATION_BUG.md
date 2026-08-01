# SESSION_ISOLATION_BUG.md — create_session 消息污染 + worker 死锁 bug

> **状态：已诊断，待走 A→B 修复。** 这是 ion 后端两个相关 bug，导致歌词工坊的循环编排 critic 评估不稳定、worker 卡死。

## Bug 1：create_session 消息污染

`create_session` 返回**全新的 session_id**，但该 session **继承了同目录（同 project_key）旧 session 的消息历史**。新建的 session 一出生就带有数十条无关消息。

## 复现（命令行可验证）

```bash
# 1. 清干净
rm -rf ~/.ion/agent/sessions/ ~/.ion/agent/last_session
ion serve &   # 起 host

# 2. 跑一次真实对话（产生消息）
ion -p "改编一句歌词" --agent lyricist

# 3. 新建一个 critic session
SID=$(ion rpc --method create_session --params '{"agent":"critic"}' | jq -r .data.session_id)
echo "新建 critic sid: $SID"

# 4. 查这个全新 session 的消息数 —— 应该是 0，实际是几十
ion rpc --session "$SID" --method get_session_info | jq .data.message_count
```

**实测结果**（2026-08-01）：两次 `create_session(agent:critic)` 返回不同 sid（`sess_f040c75d` / `sess_178feef2`），但第一个新 sid 的 `message_count=25`——一个全新 session 不可能有 25 条消息。

## 影响

歌词工坊的循环编排里，每个 critic session 都被污染：
- critic 的 user 消息里**混入了之前 lyricist 的改编 prompt**（"改编：床前明月光…输出 lyric_result"）
- critic 的上下文被无关消息污染，VERDICT 评估不稳定（同一版本反复 APPROVE→REQUEST_CHANGES→APPROVE 抖动）
- 前端循环被 critic 抖动拖着无谓拉扯到 maxRounds

证据：critic session `sess_d768a408`（agent=critic）的 user 消息序列：
```
1. 改编：床前明月光…程序员加班…输出<lyric_result>      ← lyricist 的 prompt（不该出现在 critic 里）
2. 下面是 lyricist 的改编结果…审查 VERDICT               ← critic 该收的
3. 请把下面这首歌词改编成「程序员日常」…输出<lyric_result>  ← 又是 lyricist prompt
4. 下面是 lyricist 的改编结果（第 1 版）…审查            ← critic 该收的
5. 下面是 lyricist 的改编结果（第 2 版）…审查            ← critic 该收的
```

## 推测的根因

session ID 是新生成的，但**消息存储的定位**用了错误的 key。可能位置：

1. **`do_create_session`（`src/bin/ion.rs:4132`）**：`cfg.session = Some(session_id.clone())` 设了新 sid，但 worker spawn 时消息文件的路径可能基于 **project_key（cwd-hash）** 而非 session_id，导致同 cwd 下的新 session 读到了同目录旧 session 的 JSONL。
2. **SessionIndex（`src/session_index.rs`）**：`load()` 从磁盘读 `sessions.index.json`，新建 session 时如果 index 里已有同 project_key 的条目，可能错误关联。
3. **`session-titles.json`**：`auto_session_title` 用 `turn_0` 作 key（不是 session_id），多 session 共享导致标题串台。

需要 developer 在隔离环境 debug `do_create_session` → `prepare_worker_spawn` → worker 加载消息的完整链路，定位"新 sid 为何读到旧消息"。

## 修复方向（给 developer）

1. **确认 session 消息存储路径**：JSONL 文件名/目录必须**只按 session_id 定位**，不能按 project_key 共享。新建 session 的消息文件必须是空的。
2. **`do_create_session` 后置校验**：create 完立即 `get_session_info`，`message_count` 必须 ≤ 1（仅 system prompt），否则报错。
3. **加单元测试**：连续 create_session 两次，断言两个 sid 的 message_count 都为 0/1。

## A→B 执行

```bash
ion --host --agent coordinator "按 docs/testing/SESSION_ISOLATION_BUG.md 修复 create_session 消息污染 bug：
1. 定位 do_create_session → worker 消息加载链路里 session_id 与 project_key 混用的地方
2. 确保新 session 的消息存储完全隔离（只按 session_id）
3. create 后 message_count 必须 ≤ 1，加断言
4. 补单元测试 + cargo test --lib 全过"
```

## 验证（修复后）

```bash
rm -rf ~/.ion/agent/sessions/ ~/.ion/agent/last_session
ion serve &
ion -p "测试" --agent build
SID=$(ion rpc --method create_session --params '{"agent":"build"}' | jq -r .data.session_id)
# 断言：新 sid 的 message_count <= 1
[ "$(ion rpc --session $SID --method get_session_info | jq .data.message_count)" -le 1 ] && echo "✅ 隔离正常" || echo "❌ 仍污染"
```

---

## Bug 2：worker 死锁（abort 无效）🔴 更严重

### 现象

多个 session 卡在 `Busy` 状态出不来。调 `abort` RPC 返回 `success:true`，但 session **状态不变**（还是 Busy），worker 主循环死锁，只能重启 host 清理。

### 复现（2026-08-01 实测）

歌词工坊循环编排跑几轮后：
```
$ ion rpc --method list_sessions | jq '.data.sessions[].status'
"Busy" "Busy" "Busy" "Busy" "Busy" "Busy"   # 6 个全 Busy
$ ion rpc --session <sid> --method abort | jq .success
true                                          # abort 报成功
$ ion rpc --method list_sessions | jq '.data.sessions[].status'
"Busy" "Busy" "Busy" "Busy" "Busy" "Busy"   # 状态没变！
```

### 影响

- worker 卡死后占用资源，host 被僵尸 session 拖垮
- `abort` 不可靠 → 用户无法自救，只能重启 host（丢失所有 session）
- 与 Bug 1 叠加：污染的 session 容易触发 worker 死锁（上下文混乱导致 agent 反复重试/死循环）

### 推测根因

1. **`abort` 只发信号不等生效**（`src/bin/ion.rs:5366` 附近 fire-and-forget）：abort 命令进了 worker 的 stdin 队列，但 worker 主循环卡在某个 `await`（LLM 调用重试 / bash 执行 / 工具调用）上，**永远读不到 abort 信号**。
2. **LLM 调用无超时**：worker 调 provider 时如果没有 per-request timeout，遇到慢响应/死循环重试会无限期阻塞主循环。
3. **工具执行无强制中断**：bash/工具执行时，abort 无法打断正在跑的子进程。

### 修复方向（给 developer）

1. **abort 必须能强制中断**：要么给 worker 主循环加 cancellation token（tokio `CancellationToken`），要么在 abort 时直接 kill 卡住的子任务。
2. **LLM 调用加超时**：每个 provider 请求加 max timeout（如 120s），超时则报错让 agent 决定重试还是放弃，不能无限等。
3. **watchdog 机制**：host 监测 session Busy 超过 N 分钟且无活动，自动标记 Stale + 释放 worker。
4. **abort 后置校验**：abort 后等待 3s，再查状态；若仍 Busy 则升级为强制 kill worker。

## A→B 执行（两个 bug 一起修）

```bash
ion --host --agent coordinator "按 docs/testing/SESSION_ISOLATION_BUG.md 修复两个 bug：

【Bug 1: session 消息污染】
1. 定位 do_create_session → worker 消息加载链路里 session_id 与 project_key 混用
2. 新 session 的消息存储完全隔离（只按 session_id）
3. create 后 message_count 必须 ≤ 1，加断言

【Bug 2: worker 死锁 abort 无效】
4. abort 能强制中断卡住的 worker（CancellationToken 或 kill 子任务）
5. LLM 调用加 per-request timeout（120s）
6. abort 后等 3s 复查，仍 Busy 则强制 kill

【测试】
7. 补单元测试：连续 create_session 两次 message_count 都 ≤ 1
8. 补测试：abort 一个 Busy session 后 3s 内转 Stale/Dead
9. cargo build + cargo test --lib 全过"
```
