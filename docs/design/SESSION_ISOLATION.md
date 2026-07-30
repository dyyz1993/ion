# 会话隔离与 Session GC

> **状态：已验证** — 主会话默认隔离到 `<sid>.jsonl` + 启动时 GC 清理旧文件。两个 PR（#35 隔离核心 + #36 GC）已 merge，965 lib tests + 手动验证 + e2e 集成测试全过。

---

## 1. 解决什么问题

### 问题 1：主会话 JSONL 共享（93MB 事件根因）

改造前，同一个工作目录（cwd）下的**所有主会话**都 append 到同一个固定文件 `~/.ion/agent/sessions/<cwd_hash>/session.jsonl`。后果：

- 会话之间**互相能看到对方的消息历史**（无隔离）
- 文件**无限膨胀**——反复跑会话（特别是 CI / 长会话）一直 append，最终长到 93MB / 11 万条消息
- 新会话启动时**全量加载**这个文件，CPU 100% 卡死（还触发了 `count_live_messages` 的 O(n²) 性能 bug）

### 问题 2：无 GC（5150 目录 / 1.7MB 索引）

完全没有清理代码。`SessionIndex` 只增不减，session 目录无限堆积。实测一台机器上有 5150 个 session 目录、1.7MB 索引文件。

### 问题 3：distillation 读全局 last_session（并发泄漏）

会话结束时的 skill distillation 从全局文件 `~/.ion/agent/last_session` 读 session_id。并发会话场景下，A 会话 shutdown 时读到的可能是 B 会话刚写的 id → 提炼错误会话。

---

## 2. 解决方案

### 2.1 主会话默认隔离（PR #35）

每次 `ion` 启动（无 `--continue`/`--resume`）默认开一个**新的 `<sid>.jsonl` 文件**，不再 append 到共享的 `session.jsonl`。

**改动链路**（6 个文件）：

| 文件 | 改动 |
|------|------|
| `src/bin/ion.rs` `cmd_run` | 无 session_id 时生成新 `sess_<uuid8>`（不再复用 `session.jsonl` header id）；设 `set_session_file_override` + `ION_FORK_CHILD=1` 让所有 session IO 走 `<sid>.jsonl` |
| `src/bin/ion.rs` `save_session` | 读写用 `resolve_session_file`（honors override）而非写死 `session_path` |
| `src/bin/ion.rs` `find_most_recent_session` | 扫每个 cwd 目录的**所有 `*.jsonl`**（按后缀过滤，比硬编码文件名健壮） |
| `src/bin/ion.rs` `load_session` | 新增 Strategy 3：扫 cwd 目录精确匹配 `<id>.jsonl`，让 `--resume <id>` 能找到隔离文件 |
| `src/session_jsonl.rs` `ensure_session_header` | 用 `resolve_session_file`（受 override 影响），header 写进正确文件 |
| `src/agent/tool.rs` `get_messages` | 读 session 走 `resolve_session_file` |

**distillation 修复**（同 PR）：

| 文件 | 改动 |
|------|------|
| `src/agent/extension.rs` `SessionContext` | 加 `session_id: Option<String>` 字段 |
| `src/agent/agent_loop.rs` | startup + shutdown 构造 SessionContext 时填入 `self.session_id` |
| `src/learning_extension.rs` `on_session_shutdown` | 用 `ctx.session_id` 替代读全局 `last_session` 文件（修复并发泄漏） |

### 2.2 Session GC（PR #36）

启动时异步跑一次 GC（仿 `file_snapshot/gc.rs`），不阻塞 agent。

**策略**（per cwd session 目录）：
1. 删 mtime 超过 `max_age_days`（默认 30 天）的 `*.jsonl`
2. 每个目录超过 `max_sessions_per_cwd`（默认 50）的，按 mtime LRU 删最旧的
3. 删空的孤儿目录
4. 同步清理 `sessions.index.json` 里对应条目（`SessionIndex::remove`）

**保护**：当前活跃 cwd 的目录**永不删除**。

**新模块**：

| 文件 | 内容 |
|------|------|
| `src/session_gc.rs`（新） | `SessionGcConfig` + `run_gc` + `collect_jsonl_files` + `read_header_id` |
| `src/session_index.rs` | 加 `len` / `is_empty` / `remove` |
| `src/config.rs` | 加 `SessionConfig`（`max_age_days` / `max_sessions_per_cwd` / `gc_on_start`），全 `#[serde(default)]` |
| `src/bin/ion.rs` `cmd_run` | agent config 构建后，后台线程 spawn `run_gc` |

---

## 3. 配置

`~/.ion/config.json`（可选，全字段都有默认值，不写就用默认）：

```json
{
  "session": {
    "max_age_days": 30,
    "max_sessions_per_cwd": 50,
    "gc_on_start": true
  }
}
```

| 字段 | 默认 | 说明 |
|------|------|------|
| `max_age_days` | 30 | 删除 mtime 超过此天数的 session 文件 |
| `max_sessions_per_cwd` | 50 | 每个 cwd 目录最多保留多少 session，超出 LRU 删 |
| `gc_on_start` | true | false 则启动时不跑 GC |

---

## 4. 隔离矩阵（改造后）

| 数据层 | 同 cwd 多会话 | 不同 cwd | 隔离 |
|--------|--------------|---------|------|
| **主会话 JSONL** | ✅ 各自独立 `<sid>.jsonl` | 各自 | ✅ 已修复 |
| **fork/子 worker JSONL** | ✅ 各自独立 | 各自 | ✅ 本来就隔离 |
| **session 内分支** (leaf_pointer) | ✅ 软过滤 | n/a | ✅ |
| **memory 内容** (global-memory.db) | ⚠️ 按 project_name 共享 | 同名目录会撞 | 设计如此 |
| **提炼的 skill** (skills/) | ⚠️ 全局共享 | ⚠️ 全局共享 | 设计意图 |
| **distillation 读 session_id** | ✅ 从 context 传参 | n/a | ✅ 已修复 |

**仍存在的共享**（设计如此，非 bug）：
- memory 按 project_name 共享（跨会话记忆是 feature）
- skill 全局可见（可复用技能是 feature）

---

## 5. 项目状态：未上线，无需兼容旧数据

> AGENTS.md 明确规定：本项目未上线，所有数据格式 breaking change 可直接做，不需要写迁移逻辑。旧 session 文件（包括之前那个 93MB 的）可直接 `rm -rf ~/.ion/agent/sessions/` 清理。

- **旧 `session.jsonl` 文件**：无需兼容。`find_most_recent_session` 扫所有 `*.jsonl` 是正确做法（按后缀过滤比硬编码文件名健壮），不是为了兼容旧文件。开发产生的旧 session 直接清即可。
- **`--continue` / `--resume`**：多 strategy 查找是正常的兜底逻辑（session 可能存在不同位置），不是兼容层。
- **config.json**：`SessionConfig` 全字段 `#[serde(default)]` 是 serde 标准实践，不是兼容性妥协。
- **Session GC**：开发期可直接 `rm -rf ~/.ion/agent/sessions/`；GC 是上线后才需要的自动清理。

---

## 6. 验证

### 手动验证（隔离）

```
$ cd /tmp/test && ion "msg one" -p --model faux --provider faux
$ cd /tmp/test && ion "msg two" -p --model faux --provider faux
$ ls ~/.ion/agent/sessions/<hash>--test--/
  sess_1b51b6a7.jsonl   ← 各自独立文件
  sess_7d29f98c.jsonl   ← 不再共享 session.jsonl
```

### 手动验证（--continue）

```
$ cd /tmp/test && ion "remember MAGIC12345" -p --model faux --provider faux
$ cd /tmp/test && ion --continue "what did I say" -p --model faux --provider faux
  → 基于历史回答（MAGIC12345 被加载）
```

### 测试

- **965 lib tests** passed（含 6 个新 GC 测试 + 2 个 ensure_session_header override 测试）
- **e2e 集成测试**：在隔离的 `ION_SESSION_DIR` 里造旧文件 + 超量文件，确认 `run_gc` 删对了
- **Linux CI**：PR Gate + build-and-test 全绿

---

## 7. 未做（后续工作）

| 项 | 原因 |
|----|------|
| memory 跨项目 basename 碰撞 | 设计如此（跨会话记忆是 feature），单独处理 |
| skill 全局可见性 | 设计意图（可复用技能），单独处理 |
| `tree_store::no_change_turn_zero_overhead` 偶发失败 | static SEQ 测试污染，独立问题 |
| 旧 `session.jsonl` 主动迁移 | 用户选择"不管"，靠 GC 清 |

---

## 8. 相关 PR

- **PR #35** — `feat(session): isolate main session to per-run <sid>.jsonl files`
- **PR #36** — `feat(session-gc): clean up old session files at startup`
- **PR #34**（前置）— `fix(file_snapshot): resolve Linux CI flaky approval tests`
- **PR #33**（前置）— `chore: cargo fmt + relax clippy to unblock PR Gate CI`
