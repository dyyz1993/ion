# ION 项目状态

> **状态：开发中** — 未上线，无需兼容旧数据。最近盘点：2026-08-25。
>
> 本文件是**当前快照**（功能清单 + 测试统计 + 路线图）。历史 commit 看git log`，详细设计看 `docs/design/`。

---

## 一、规模快照（2026-08-25 实测）

| 维度 | 数值 |
|------|------|
| Rust 代码总行数 | **~100,500**（src ~83,700 + ion-provider ~5k + file_index ~300 新增） |
| lib 测试 | **984 passed / 0 failed**（含 file_index 3 新增；hooks 2 个旧失败已修复） |
| 文档 | 112 篇 .md |
| 设计文档 | `docs/design/` 40+ 篇 |

### 2026-08-25 新增

| 模块 | 内容 |
|------|------|
| **FileIndex**（`src/file_index.rs`） | 会话 JSONL 稀疏偏移索引：一次扫描建索引（偏移/len/type/id/parentId/role/220字预览/targetIds）+ Live 视图预计算消息下标序列 + append-only 增量 refresh + `read_at` 按需解析单行。基准 179MB/80K 行：**缓存命中 get_messages 5ms / head 4ms（vs 旧路径 1245ms → 250x 加速）**，峰值内存 12MB（vs 361MB → 30x 内存降） |
| **ion-console**（`~/Project/study-rust/ion-console`） | zcode 风格 Web UI（Vite+React+TS+Tailwind v4+Lucide），Node 网关桥接 host Unix socket。侧栏项目分组+搜索+worker 状态点+⋮/右键菜单（复制名称/ID）+折叠图标轨；会话视图 host 直读（默认底部/before 翻页/from=head 顶部直跳/浮动回底 FAB）+每轮文件记录行+分型工具卡（bash 终端风/edit diff/write 代码块）+Markdown+步骤摘要折叠+乐观渲染+WS 实时流+生成中打断（Stop 按钮）；审查工作台（待审聚合+轮次大纲+随时批准/拒绝回滚+三基准 diff：轮内/上一轮/磁盘）；设置栏模型/权限/思考/上下文环形图。42 项 Playwright UI 测试 |
| **协议四补全** | get_session_messages/list_session_turns 并入 `--session` 直读拦截；`get_messages` 新增 `from=head`；list_inputs/get_turn_detail host 直读（40ms 零拉起）；subscribe 改 session 级绑定（worker 未拉起挂起等待 60s 自动接上 + 死亡自动重接发 `resubscribed`） |
| **turn_file_diff RPC** | 单 turn 单文件 diff，base=before/prev/disk 三种对比基准 |

### 2026-08-25 修复的 10 个 bug

1. SessionIndex upsert 整替换清零 meta（name/计数/created_at）→ 兜底保护
2. register 复活不带原值 → merge_existing_meta（含 project 归属）
3. save_worker_session 重放污染（历史以当前时间戳整段重写）→ 指纹防护
4. write 相对路径裸文件名拼 `/.ion-tmp-` 写只读根 → resolve 到 cwd
5. RPC auto-create 不传 project_path → 消息落错项目目录 → SessionIndex 兜底
6. StepSnapshot 无 session 归属 → 审批 baseline 用项目史上第一个快照（705 文件根因）
7. scanner 不忽略 Python 缓存目录 → .ruff_cache 等混入快照
8. get_context_usage 空闲合成用累计 token_input（虚高）→ 改用最后一次 LLM usage.input
9. context_window 查不到 → 三级查找 registry→provider/id→config.json
10. subscribe 先于 worker 建立即失效 → session 级绑定

---

## 二、已完成功能清单

> 仅列**已落地**的功能。设计/排期中的看路线图。

### 核心内核

- **CLI** — 45+ 参数（对齐 pi 41 核心）。单一二进制 `ion`，`--mode rpc` 进 worker 模式
- **Provider 抽象** — `ion-provider` 独立 crate，OpenAI SSE + tool_calls，4 provider + 9 协议（含 Azure/Codex/Vertex）
- **Agent 循环** — 内外两层 + 约 45 个 Extension trait 方法 + 23 已接入
- **27 个内置工具** — read/write/edit/bash/grep/find/ls/calculator/echo + 7 Git + spawn/send/resume/await/channel_send/kill + global_memory_search/save + branch_session + remote tool
- **会话管理** — JSONL v3 + 实时索引 + fork/continue/resume + cwd-hash 分组
- **三场景引擎** — 场景 1 直接执行 / 场景 2 临时 host / 场景 3 常驻 host
- **多智能体编排** — 7 个工具（spawn_worker/resume/send/await/kill/channel_send + send_to_session），同步异步全覆盖

### 扩展系统

- **WASM Extension** — `WasmExtensionRegistry` 热更新 + ABI v1 + 4 维数据存储；内置生命周期由 `ExtensionRunner` 调度
- **Extension Host API** — `ctx.fs` 统一文件访问 + `safe_join` 路径逃逸防护
- **5 类 hooks 系统** — command/http/prompt/agent/mcp_tool（2975 行，热重载）
- **MCP 系统** — rmcp 1.x + host→worker 工具发现 + 重连监控 + resources/prompts
- **Memory V0.2** — 单例扩展 + SQLite/FTS5 + 跨项目检索 + 中文 LIKE fallback
- **File Snapshot** — 双路快照（object + tree）+ parented `step-snapshot` + tree-hash restore + zstd 压缩 + 三级 GC + approval
- **Goal Supervisor** — `on_gate_check` 证据驱动目标闭环 + 6 道防线 + 趋势分析 + goal_refine/diagnose
- **Goal Evolver** — 日志分析进化（3 维度：deadloop/model/context）→ Issue 计划
- **LSP Extension** — 5 语言诊断（Rust/TS/Python/Go/HTML，基于编译器 JSON 输出，非 rust-analyzer）
- **Rules Engine** — `.ion/rules/*.md` frontmatter glob 匹配 → 注入 XML
- **Learning Extension** — Secret Detector + 会话分析 + Skill Distillation（LLM 提炼）
- **Monitor Extension v2** — 定时监控→LLM 触发 + self-healing pipeline
- **Plan 工具** — 内置 6 工具 + strict_mode 强制审批
- **PermissionProfile** — 5 模式（permissive/readonly/standard/strict/autopilot）
- **Stored-Decision 权限记忆** — "always allow" 持久化

### 其他

- **Session Tree** — 文件内分支 + leaf 指针 + only-append 回滚
- **Compaction** — 分批并发 + LLM summarizer + emergency fallback
- **Message Retrieval** — 9 接口 + 分页/视点/过滤/turn 聚合
- **Record/Replay** — LLM 决策录制回放（复用 FauxProvider）
- **FauxProvider** — 架构级 LLM Mock（FIFO 队列 + 工厂响应 + 流式分块）
- **Worker 崩溃恢复** — stderr 捕获 + exit code + Dead 保留 + 父通知
- **HTML Export** — ION 自有单文件离线模板 + 会话元信息卡 + Flow Summary + tools 列表 + 完整可见事件 Timeline（17 种固定 Entry、25 种已识别内置 Custom、当前会话类型统计、运行时 Extension 开放类型、筛选/悬停/点击跳转）；真实消息、Compaction 与 parented File Snapshot 在正文/Timeline 一一对应；仅当隐藏正文超过 3 行时折叠，预览保留 3 行正文（`tests/export_ci.sh` 54/54）
- **Apple Container 后端** — 真隔离 Linux VM，同端口并行
- **A→B 自进化** — A 调度 B 改代码 + CI + 合并 + PR（14 脚本，24 agent 模板）
- **Host 级会话直读** — `get_session_messages` / `list_session_turns`：host 纯磁盘读 JSONL（append-only 线性增长，冷读毫秒级），UI 浏览历史会话零 worker；旧命令名 `get_messages`/`list_turns` 带 session 自动拦截直读、不 auto-create worker；空闲会话状态合成——get_session_info/get_settings/get_queue/get_context_usage/get_active_tools 无 worker 时从 SessionIndex/全局配置合成响应、不 auto-create，有 worker 照旧转发
- **FileIndex 长会话渲染** — 稀疏偏移索引 + Live 视图预计算 + `read_at` 按需解析 + O(1) 切片翻页。179MB 基准：get_messages **5ms**（vs 旧路径 1245ms → 250x）；峰值内存 12MB（vs 361MB → 30x）。`src/file_index.rs`
- **ion-console** — 独立 Web UI 项目（`~/Project/study-rust/ion-console`，Vite+React+Tailwind+Lucide），Node 网关桥接 host。会话浏览/续聊/审查审批/文件记录/实时流/打断。42 项 Playwright UI 测试

---

## 三、A→B 自进化（核心特色）

**铁律：A 只调度，B 改代码。** A 永远通过 `container exec B ion --agent developer` 让 B 在隔离环境动手。详见 [docs/design/SELF_EVOLUTION.md](./design/SELF_EVOLUTION.md) + [docs/design/EVOLVER_LESSONS_LEARNED.md](./design/EVOLVER_LESSONS_LEARNED.md)。

**累计验证**：14 个 A→B 闭环任务，39 个测试全过，0 个 U+FFFD 残留。

**6 道守门机制**：① U+FFFD 守门 ② Cargo.toml 守门 ③ Reviewer agent ④ cargo build ⑤ cargo test --lib ⑥ cargo clippy。

**关键脚本**（`scripts/evolve*.sh`，活跃；已归档的在 `scripts/archive/`）：
- `evolve.sh` — 启 container + volume cache 编译
- `evolve_self.sh` — 串行批量（B 改 ION 自己源码）
- `evolve_pr.sh` — B 改代码 → GitHub PR → merge
- `evolve_verify.sh` — 独立 CI 验证
- `init-evolve-container.sh` — container 初始化（装 Rust + 复制 ion binary）
- `auto_evolve_local.sh` — 本地化 A→B 自循环（不用 container）

---

## 四、测试统计（2026-08-25 实测）

| 套件 | 数量 | 覆盖 |
|------|------|------|
| **lib tests** | **984 passed / 0 failed** | 全部核心逻辑（含 file_index 3 新增） |
| unit_rpc_test | 20 | RPC 协议 U1-U20 |
| wasm_extension_tests | 24 | ABI、工具调用、热更新、4 维存储与 Plan 生命周期 |
| extension_cli_ci | 16 | install/remove/list/create + 可构建脚手架 |
| extension_fs_ci | 23 | ctx.fs、安全边界与 `extension_id` 存储隔离 |
| rpc_event_push_ci | 18 | 用户触发的每条 RPC 推送 `rpc_response` 事件 + 权限变更 `permission_changed` 类型化事件 + 双终端实时同步 |
| host_read_ci | **20** | Host 级会话直读 + 协议四补全（E 组 5 新增：--session 路由/from=head/list_inputs/get_turn_detail/零拉起） |
| ion-console UI | **38** | Playwright：侧栏/视图/折叠/面板/审批/记录行/viewer/FAB/复制/预启动（`npm run test:ui`） |
| CI 脚本 | 30+ 个 `tests/*_ci.sh` | CLI 外部验证（MCP/hooks/extensions/snapshot/goal/memory 等） |

---

## 五、路线图

**P0（当前）**：~~CommandGuard + 权限引擎~~ ✅ 完成
**P1（Runtime 抽象）**：✅ 完成（Local/Sandbox/Remote/AppleContainer + BackendRegistry 路由层）
**P2（路径权限）**：✅ 完成（CommandGuard 三模式 + PermissionRule 热重载）
**P3（UI 对接）**：✅ 完成（subscribe --ui + ui_respond + audit.jsonl）
**P4（扩展生态）**：✅ 完成（ExtensionApi + WASM 钩子 + emit 自定义事件）
**P5（包管理）**：✅ 完成（`ion extension install/remove/list`）

**待办（无优先级）**：
- ~~修 2 个 hooks 失败测试~~ ✅（lib 984/0 全绿）
- scripts/ 目录归档（evolve 系列 14 个 → 留核心 3-4 个）
- 105 个 TODO/FIXME 筛查（多数是测试 fixture，真技术债约 20-30 处）
- 快照 store 按 git common dir hash 分裂（同会话散两个目录）
- abort 对阻塞 LLM 请求无效（需 reqwest 超时）
- SessionIndex messageCount 不可靠（大量为 0）

---

## 六、推荐模型配置

| 用途 | 模型 | Provider | 说明 |
|------|------|----------|------|
| **B 改代码（主力）** | `glm-5.2` | `zai` | UTF-8 稳定（无 U+FFFD）、推理强 |
| **快速测试** | `deepseek-v4-flash` | `opencode` | 便宜快，适合 CI |
| Avoid | claude-opus / gpt-4o | — | 昂贵 |

配置示例见 [AGENTS.md §模型配置](../AGENTS.md)。
