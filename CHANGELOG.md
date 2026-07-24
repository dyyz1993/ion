# Changelog

All notable changes to ION are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/).


## [0.3.0] — 2026-07-24

### Bug Fixes
- **compaction max_tokens**: set to 32000 (was None) — fixes DeepSeek reasoning model 400 errors
- **WASM WASI stubs**: added environ_get/fd_write/proc_exit/random_get/clock_time_get — std-compiled WASM extensions now load
- **evolve_auto.sh stdin**: fixed task transfer via printf to file (B was not receiving tasks)
- **permission hot-reload**: auto-reload settings.json on mtime change (rules now update without restart)
- **SSE idle timeout**: 120s timeout for stalled connections (zai proxy reasoning phase)

### New Features
- **DeepSeek fast model verified**: A→B self-evolution works with DeepSeek-V4-Flash
- **evolve_loop.sh**: continuous self-evolution loop (self_test → fix → verify → repeat)
- **WASM Extension Guide**: docs/guides/WASM_EXTENSION_GUIDE.md (689 lines, by A→B)
- **evolve_auto.sh publish stage**: auto version bump + changelog + git tag

### Verification
- Permission deny→grant→deny roundtrip verified (hot-reload works)
- 3 WASM extensions load successfully
- DeepSeek + GLM-5.2 both verified working
- 777 lib tests + 15 ion-worker tests pass

## [0.2.0] — 2026-07-24

### 🚀 新功能 / New Features

#### WASM 扩展生态（用户可 DIY）
- **rules-engine WASM 扩展** — 扫描 `.ion/rules/*.md`，按文件类型自动注入项目规则到 system prompt。安装：`cp extensions/rules-engine/rules_engine.wasm ~/.ion/agent/extensions/`
- **file-time-guard WASM 扩展** — 追踪文件修改时间，拦截对过期文件的写入（防止 agent 用旧上下文覆盖用户改动）。安装：`cp extensions/file-time-guard/file_time_guard.wasm ~/.ion/agent/extensions/`
- **session-supervisor WASM 扩展** — agent 完成后自动扫描 TODO/FIXME 残留，强制 agent 继续修复。安装：`cp extensions/session-supervisor/session_supervisor.wasm ~/.ion/agent/extensions/`

#### 上下文压缩（Context Reclaimer）
- **优先级 token 回收** — 每轮自动剥离 thinking blocks（省 30-60% token），超阈值时按优先级回收旧工具输出（bash > grep > read）
- **热度窗口保护** — 最近消耗窗口 30% token 的消息保持完整（支持回滚/对比），超出的才回收
- **Stale read 检测** — 文件被 write/edit 改过后，旧 read 结果自动回收（磁盘已是新版本）
- **实测效果**：50 轮对话场景节省 73.9% token

#### WASM 内核能力补全（开发者关注）
- 新增 9 个 host functions 让 WASM 扩展能访问 agent 状态：
  - `host_get_token_count` — 查当前 token 用量
  - `host_get_messages` — 读完整对话历史
  - `host_get_state` — 读 agent 状态（model/queue 长度等）
  - `host_steer` — 注入 steer 消息（强制 agent 继续）
  - `host_inject_follow_up` — 注入 follow-up
  - `host_llm_call` — 调小模型做推理
  - `host_get_worker_status` — 查子 worker 状态
  - `host_compact_now` — 立即触发压缩
  - `host_create_worktree` — 创建 git worktree
- 新增 3 个文件系统 host functions：`host_write_file`、`host_path_exists`、`host_glob`
- WASM host function 总数：17 → **27 个**
- WASM 生命周期钩子：**36 个**（全覆盖）

#### Watchdog 安全升级
- **心跳监控** — 每 5 秒发 health RPC，3 次连续失败判定死亡（检测死锁/OOM/hang）
- **自动回滚** — 升级失败时从备份 binary 恢复 + 重启
- **三种故障场景验证通过**：编译失败 / 启动崩溃 / 进程卡死

#### 一条命令自进化
- `bash scripts/evolve_auto.sh "任务描述"` — 完整闭环：A 调 B 改代码 → 守门 → GitHub PR → watchdog 自动升级 → 清理

#### Memory V0.3
- 新增 entities + relations 两张表（知识图谱）
- 12 个新方法：add/get/list/update/delete entity + relation CRUD + BFS find_path

### 🐛 修复 / Fixes
- **SSE 空闲断连** — zai API proxy 在 reasoning 阶段断 SSE 连接导致 B 卡死。新增 120 秒空闲超时检测
- **BusyBox grep** — container 里 Alpine 的 grep 不支持 `--include`，Dockerfile 加装 GNU grep
- **health RPC 路由** — B 把 health 放在了 Worker 级，移到 Manager 级（`ion rpc --method health` 现在能调通）

### 📦 安装 / Install
```bash
# 扩展安装（3 个 WASM 扩展，扔进去就生效）
cp extensions/rules-engine/rules_engine.wasm ~/.ion/agent/extensions/
cp extensions/file-time-guard/file_time_guard.wasm ~/.ion/agent/extensions/
cp extensions/session-supervisor/session_supervisor.wasm ~/.ion/agent/extensions/
```

### 📊 数据
- 测试：777 passed, 0 failed
- WASM host functions: 27
- WASM 钩子: 36
- 扩展（内置+WASM）: 12 内置 + 3 WASM = 15
- Agent 模板: 17

---

## [0.1.0] — 2026-07-19

### 首次发布

- CLI 45+ 参数（对齐 pi）
- Provider 抽象层（5 个 provider 协议）
- Agent 循环（内外两层 + 23 个扩展钩子接入）
- 27 个内置工具
- 会话管理（JSONL v3 + fork/continue/resume）
- Manager 守护进程 + Worker 子进程
- WASM 扩展完整链路
- Worktree 隔离
- 权限引擎 + 命令守卫
- Apple Container 后端
- Session Tree（会话分支）
- File Snapshot（双路文件快照）
- MCP 系统（Phase 1-4）
- Memory V0.2（跨项目记忆）
- Hooks 系统（5 handler 类型）
- Record/Replay（LLM 录制回放）
- FauxProvider（架构级 Mock）
- Workflow Engine（YAML DSL）
- Team 编排（agent.md 驱动）
- A→B 自进化架构
- 测试：488 Rust + CLI E2E
