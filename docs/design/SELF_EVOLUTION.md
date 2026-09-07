# Self-Evolution — A→B Architecture Overview

> **状态：开发中** — 既有 A→B 链路已有实现；24 小时无人值守闭环尚未验收。2026-09-08 审查、证据及执行队列见第 9 节。

> 第 1–8 节保留既有设计背景，其中自动合并、历史脚本路径和验证声明不作为本轮执行授权。第 9 节是本轮交接依据。

---

## 1. One-Line Summary

**A orchestrates, B writes code, A merges. A never touches code.**

A (host coordinator ION instance) drives B (container developer ION instance) to edit source, run CI, and self-verify. B's passing changes flow back to A via bind-mount + git merge. A itself is sandboxed: `edit`/`write` are `disallowed_tools`, and `CommandGuard` blocks every host-side mutation path (`sed -i`, `cat >`, `python3 -c`, etc.).

---

## 2. Architecture Diagram

```
 ZCode (user / CI)
   │
   │  ion --host --agent evolver "add fn to global_memory"
   │  (human-level task handed to A)
   ▼
┌──────────────────────────────────────────────────────┐
│  A — Host Coordinator (agent role: evolver)          │
│                                                      │
│  • git worktree add        (isolate workspace)       │
│  • container run           (spawn B + compile ion)   │
│  • container exec B dev    (drive code change)       │
│  • container exec B check  (B runs its own CI)       │
│  • gate: U+FFFD/build/test (reject or accept)        │
│  • git merge worktree      (pull B's commit back)    │
│  • container stop + prune  (cleanup)                 │
│                                                      │
│  A NEVER: edit, write, sed, cargo build on host src  │
└───────────────┬──────────────────────────────────────┘
                │  container exec ... ion --agent developer "..."
                ▼
┌──────────────────────────────────────────────────────┐
│  B — Container Developer (agent role: developer)     │
│                                                      │
│  • Full ION instance: ion binary + LLM + tools       │
│  • read / edit / bash (git, cargo check, cargo test) │
│  • git commit in /workspace (bind-mount = host WT)   │
│  • Does NOT know it's in a container                 │
└───────────────┬──────────────────────────────────────┘
                │  commits land in host worktree instantly
                ▼
        A merges → master → (optional) GitHub PR
```

---

## 3. The Three Orchestration Modes

| Mode | Script | Concurrency | Mechanism | Best For |
|------|--------|-------------|-----------|----------|
| **Serial** | `evolve_self.sh` | 1 B at a time | Sequential `for` loop; full per-task build+test | Reliability, deep verification |
| **Concurrent** | `evolve_concurrent.sh` | N B workers | `bash &` backgrounding + per-task worktree subdirs (`/workspace/wt-N`) | Throughput on independent files |
| **Native** | `evolve_native.sh` | coordinator-driven | ION's own `spawn_worker` + `resume_worker` (no bash `&`) | Pure multi-agent: coordinator → developer → reviewer |

**Serial** is the safe default. **Concurrent** trades some isolation for speed (N isolated git repos inside one container). **Native** is the "ION evolving itself with its own primitives" ideal — the coordinator agent orchestrates developer and reviewer as child workers.

All three share the same bootstrapping (`evolve.sh`): worktree → container → compile `ion` (single binary, worker mode built in via `--mode rpc`).

---

## 4. The 6 Gate Checks

Every change B makes must pass all six gates before A merges. Failure on any gate triggers rollback (or, for reviewer rejection, an auto-fix loop).

| # | Gate | Tool / Command | Fail Action |
|---|------|----------------|-------------|
| 1 | **U+FFFD scan** | `grep -c $'\xef\xbf\xbd' <file>` | Drive B to self-fix, max 2 attempts |
| 2 | **Cargo.toml integrity** | `diff` worktree vs. project | Hard reject — external dep changes forbidden |
| 3 | **Reviewer approval** | `reviewer` agent → `APPROVE` / `REQUEST_CHANGES` | `resume_worker` developer with feedback |
| 4 | **cargo build** | `cargo build --bin ion` | Rollback file, skip task |
| 5 | **cargo test** | `cargo test --lib` | Rollback file, skip task |
| 6 | **clippy** | `cargo clippy` | Warnings logged; errors block merge |

Gates 1–3 run *before* syncing to the main repo. Gates 4–6 run *after* sync, on the host. A file that fails any post-sync gate is reverted with `git checkout --`.

---

## 5. Volume Cache (The "V Scheme")

Apple Container is a Linux VM with no persistent build cache by default — first compile takes 10–20 minutes. `evolve.sh` mounts two named volumes to warm-start every subsequent container:

```bash
container run ... \
  -v ion-cargo-cache:/root/.cargo/registry \   # crate registry + source
  -v ion-target-cache:/workspace/target         # compiled artifacts
```

| Run | cargo registry | target/ | Total build time |
|-----|----------------|---------|------------------|
| 1st (cold) | empty | empty | ~15 min |
| 2nd (warm) | cached | cached | ~30 sec |
| 3rd+ (hot) | cached | cached | ~15 sec |

> **Caveat:** Apple Container volumes are *exclusive* — only one container can mount a given volume at a time. For parallel runs, use **bind mounts** (`-v /tmp/cache:/root/.cargo/registry`) instead of named volumes, or run concurrent workers *inside* a single container (the `evolve_concurrent.sh` approach).

---

## 6. Key Lessons (Top 5)

Hard-won from the full `EVOLVER_LESSONS_LEARNED.md` — these are the non-negotiables:

- **English-only comments.** Non-ASCII (Chinese) characters get corrupted into U+FFFD by some LLMs, which silently breaks `edit` tool pattern matching. The U+FFFD gate exists because of this.
- **GLM-5.2 > DeepSeek for UTF-8 stability.** GLM-5.2 (`zai` provider) produces cleaner byte output; DeepSeek occasionally mangles multi-byte chars. Default `MODEL=glm-5.2`.
- **Apple Container volume is exclusive.** Named volumes cannot be shared across concurrent containers. Use bind mounts or single-container-multi-worktree for parallelism.
- ~~**`evolve.sh` must compile `--bin ion-worker`**~~ (2026-07-26 已过时：ION 已合并为单一 `ion` 二进制，`cargo build --release --bin ion` 即可。host 通过 `current_exe() + --mode rpc` spawn 自身创建 worker。)
- **Reviewer reject → `resume_worker` for auto-fix loop.** The reviewer agent returns `REQUEST_CHANGES`; the coordinator feeds that back to the developer via `resume_worker`. Max 2 rounds before giving up — prevents infinite fix cycles.

---

## 7. GitHub PR Flow

For changes destined for the remote (not just local master), `evolve_pr.sh` extends the pipeline with GitHub:

```
B writes code (container)
   │
   ▼
Gate checks pass (U+FFFD, build, test)
   │
   ▼
A creates feature branch:  git checkout -b evolve/<timestamp>
   │
   ▼
A commits + pushes:        git push origin evolve/<timestamp>
   │
   ▼
A opens PR:                gh pr create --base master --head evolve/<timestamp>
   │   (PR body includes task desc, changed files, test count, model)
   ▼
A auto-merges:             gh pr merge --merge --delete-branch
   │   (tests already passed locally → safe to auto-merge)
   ▼
A returns to master:       git checkout master && git pull
```

The PR body is auto-generated with verification proof (gate results, test count, model/provider used), providing an audit trail for every self-evolved commit.

---

## 8. Related Documents

| Document | Purpose |
|----------|---------|
| [EVOLVER_LESSONS_LEARNED.md](EVOLVER_LESSONS_LEARNED.md) | Full problem log (11 issues + solutions) |
| [WATCHDOG_DUAL_VERSION.md](WATCHDOG_DUAL_VERSION.md) | Safe hot-reload of A after merge |
| [WORKFLOW_GATE.md](WORKFLOW_GATE.md) | Kernel delivery verification framework |
| [APPLE_CONTAINER_EXTENSION.md](APPLE_CONTAINER_EXTENSION.md) | Apple Container integration design |
| [TEAM_ORCHESTRATION.md](TEAM_ORCHESTRATION.md) | Multi-agent spawn/resume primitives |
| [CLI_ARCHITECTURE.md](CLI_ARCHITECTURE.md) | ION CLI structure (agents, tools, workers) |
| [../guides/CLI_USAGE.md](../guides/CLI_USAGE.md) | End-user CLI reference |
| [../guides/DEPLOY_ARCH.md](../guides/DEPLOY_ARCH.md) | Deployment topology |

### Key Source Files

| File | Role |
|------|------|
| `scripts/evolve.sh` | Bootstrap: worktree + container + compile |
| `scripts/evolve_self.sh` | Serial batch orchestrator |
| `scripts/archive/evolve_concurrent.sh`（已归档） | Concurrent (N parallel B workers) |
| `scripts/archive/evolve_native.sh`（已归档） | Native (coordinator + spawn_worker) |
| `scripts/evolve_pr.sh` | GitHub PR flow |
| `scripts/init-evolve-container.sh` | Standalone container init |
| `scripts/Dockerfile.evolve` | Rust toolchain image |
| `examples/agents/evolver.md` | A's agent definition |
| `examples/agents/developer.md` | B's agent definition |
| `examples/agents/reviewer.md` | Code review agent |
| `src/command_guard.rs` | Host-side mutation blockade |

## 9. 2026-09-08 项目审查与 24 小时执行计划

### 9.1 结论与范围

ION 已具备 Agent 循环、独立 Worker、会话树、RPC、事件广播、WASM Extension、Monitor、Goal Supervisor 和自进化脚本。下一阶段建议先把这些能力组成可靠的工作流，优先完成「接任务 → 隔离修改 → 外部验证 → 中断恢复 → 交付证据」。暂缓继续堆叠工具和大范围重构。

本次为规划与抽样审查，基线提交 `c0f2901`，开始时工作区干净。统计到 75 个 `tests/*_ci.sh`、26 个 Rust 集成测试文件；数量不等于有效覆盖率。未运行全部测试、真实 LLM 或 24 小时耐久测试，也未查询远端 CI 状态及分支保护规则。

本轮仅修改计划文档与导航，未实施产品修复。按 DESIGN_TEMPLATE 检查了已有文档，因此将计划合并到本文件。

### 9.2 已确认问题与待验证风险

| 优先级 | 发现 | 当前证据 | 影响 |
|---|---|---|---|
| P0 | 汇总报告有 FAIL，进程仍返回成功 | `scripts/aggregate_ci_results.sh` 结尾只输出统计；本次隔离注入 `FAIL / exit=42`，实际汇总器退出码为 `0` | 定时任务和 CI 可误判成功 |
| P0 | CI 矩阵重复执行且破坏串行分类 | `scripts/run_ci_matrix_parallel.sh:238–241` 先执行 PARALLEL_SCRIPTS，随后又执行完整 FILTERED；后面再执行 SERIAL_SCRIPTS | 每个测试被执行两次，串行测试也进入并行批次，增加耗时与互相干扰 |
| P0 | 验证链路绕过错误 | `.github/workflows/ci.yml` 的 lint/fmt/lib test 使用 `continue-on-error`；`pr-gate.yml:62` 用 `|| echo` 吞集成测试错误；矩阵 cargo shim 对 build/check/clippy/fmt 直接返回 0 | 必须区分已验证、复用已验证产物、未执行，不能统一记 PASS |
| P0 | 运行隔离不完整 | 矩阵启动时删除源工作区 `.ion/monitors`，多个运行共享 `/tmp/ci-*` 名称 | 可能影响用户监控配置，批次结果也会互相覆盖 |
| P1 | Goal 费用防线缺少累计入口 | `goal_supervisor_extension.rs` 创建目标时费用置 0，`check_guards` 比较费用上限；检索生产源码未发现此字段的累加写入 | 不能把该字段视为真实消费硬上限；现有测试只人工填入费用后测比较逻辑 |
| P1 | Goal 重启恢复与存储约定不闭合 | `worker_rpc.rs:344` 初始化目标为 None；目标执行过程另写 `goal-runs/<session>/iterations.jsonl` 和 `final-report.json`，未发现启动重放 Goal 的入口 | 长时间任务重启后连续性需要验证；会话派生轨迹应回归会话 JSONL，摘要进 SessionIndex |
| P1 | 索引持久化失败不可见 | `session_index.rs:154–208` 忽略加锁/写入/rename 错误，锁文件打开失败时走无锁更新 | 磁盘满或权限错误时可能出现 RPC 成功但重启后丢状态；故障注入尚未执行 |
| P2 | 文档与执行入口过时 | README 仍要求构建已删除的 ion-worker；SELF_EVOLUTION 引用部分已移入 scripts/archive 的脚本；旧矩阵报告日期为 2026-07-29 | 小模型照文档工作容易走错入口；旧统计不能当当前质量证据 |

另一个需要控制的维护成本是 `src/worker_rpc.rs` 已有 7,993 行。但应先锁定 RPC 行为测试，再按职责拆分；第一天不安排大规模拆文件。

旧 `scripts/evolve_tasks.sh` 主要是 getter、计数函数和随机数工具等任务，不是当前用户链路的缺陷队列。`auto_evolve_local.sh` 默认自动合并，仅以 lib 测试等作为门槛，缺少每项功能的 RPC 闭环。本轮不直接复用它开始改代码。

### 9.3 本次实际验证

```bash
cargo test --test input_origin_harness --test origin_gate_harness --test origin_hide_harness --offline
```

结果：3 passed / 0 failed，包含输入来源传入钩子、monitor 来源拒绝工具、从 Provider 请求隐藏工具。编译约 40 秒，出现 1 个测试文件 unused import 警告。本次通过只覆盖这三项行为。

另在独立临时目录复制汇总脚本，仅替换其输入和输出路径，注入一条失败记录：报告显示 `PASS=0 / FAIL=1`，脚本退出码仍为 `0`。没有运行会删除 `.ion/monitors` 的完整矩阵，没有触碰用户 host。

### 9.4 执行模型与成本规则

1. 测试枚举、执行、超时、统计和去重由程序完成，正常通过时无需额外 LLM 分析。
2. 每次唤醒只读上一轮摘要、当前任务涉及的文件和失败日志。默认一个执行者、一个修复任务，不用模型协调一批仅仅运行命令的执行者。
3. 可选执行方式：用户指定 `gpt-5.4-mini` 承接独立定时工作；或 ION 已有模型配置承接。当前没有核验各账户额度或 API 报价，不承诺具体金额。
4. 未选模型时保持自动化暂停；不得自动回退到当前高成本模型。真实 Provider 测试未获明确金额/调用次数预算前只编写受控 case，不调用付费模型。
5. 每题最多两次修复尝试；复杂的并发/存储/权限问题留下复现和最小方案，由用户决定是否升级模型。费用未知时记录 unknown，不能记为 0。

### 9.5 可领取任务卡

以下是待实施任务，不代表本轮已完成。时长为单人工作槽位估算，超时就交接，不保证第一天全部修完。**T01/T02 已在第 1 轮完成（见 §9.9）。**

| ID / 槽位 | 文件范围与具体交付 | 验收条件 | 依赖 |
|---|---|---|---|
| T01 / 1–2h ✅R1 | `scripts/aggregate_ci_results.sh`：失败向上返回；显式输入清单与结果去重规则 | 注入 PASS 返回 0；FAIL、缺失、畸形记录返回非 0；同名多次运行保留 attempt 信息；不能由后一次通过抹掉历史失败 | 无 |
| T02 / 1–2h ✅R1 | `scripts/run_ci_matrix_parallel.sh`：删除重复调度，分离并行与串行；隔离运行目录和监控配置 | 用无 LLM 的假脚本记录启动次数/并发度：每项恰好一次、串行组最大并发 1；源 `.ion/monitors` 原样保留；不同 run 不共享输出 | T01 |
| T03 / 1–2h ✅R2 | `.github/workflows/ci.yml`、`pr-gate.yml` 及矩阵入口：可信验证门槛 | 人为失败能使 job 失败；环境依赖明确 skip 原因；预编译产物记录 SHA，shim 不得伪造 check/clippy/fmt 成功；Linux 特有失败隔离成有理由的已知问题 | T01–T02 |
| T04 / 2–3h | `src/goal_supervisor_extension.rs` 和必要的运行时 usage 入口：真实预算接线 | 用 FauxProvider Factory 注入可计量 usage，正常运行累计增加，重试也计入，不重复计费；下一次调用前判断限额；无价格信息时不声称预算有效；提供 RPC/Pull 和事件证据 | T03 |
| T05 / 2–3h | Goal 状态、`src/worker_rpc.rs` 恢复入口、`SessionIndex`：Goal 中断恢复 | 目标设置后结束 Worker，恢复同会话时目标/迭代/截止时间一致；custom 用 data；完整轨迹进会话 JSONL，小摘要进索引；不新增会话 sidecar；两个客户端状态一致 | T03，预算恢复依赖 T04 |
| T06 / 2–3h ✅R2 | `src/session_index.rs` 与必要 RPC 调用者：持久化错误可见 | 权限拒绝、写入失败、并发更新的故障注入可观察；失败不可返回成功；多进程写不同字段不丢更新；不得再以默认空索引覆盖损坏数据 | T03 |
| T07 / 1–2h | README、既有设计与测试导航：按当前可执行入口校准 | 删除 ion-worker 构建入口，核对 archive 路径，带日期/SHA 的新基线与旧报告区分；不把旧缺口直接当未修 bug | 无，可在阻塞时领取 |
| T08 / 余下时段 | RPC 耐久验证及结果报告 | 覆盖创建→prompt→abort→恢复、set_model→空闲→get_session_info、分支切换、订阅断连重接、两个客户端审批状态同步；每次验证有退出码、断言、耗时、进程/资源快照 | T01–T03；Goal 恢复场景依赖 T05 |

每张代码任务卡还必须满足 AGENTS.md：先 Harness；有动态分支时用 FauxProvider Factory；修复后立即用对应 `ion rpc` 查结果；补 `#[ignore]` + `ION_E2E=1` 真实 case；更新已有设计文档、CLI 验证说明、测试脚本和 AGENTS.md 统计。脚本/CI 本身的改动以故障注入验证其退出码与调度行为，不为凑数量添加无关 Agent 测试。

测试起点（执行者应先审查脚本的清理动作，再在独立环境运行）：

```bash
cargo build --bin ion --locked
cargo test --lib --locked
cargo test -p ion-provider --locked
cargo test --test input_origin_harness --test origin_gate_harness --test origin_hide_harness --locked
cargo clippy --lib -- -D warnings
cargo fmt -- --check
```

RPC 场景可从既有 `tests/host_read_ci.sh`、`tests/branch_tree_ci.sh`、`tests/session_workspace_ci.sh`、`tests/abort_ci.sh`、`tests/origin_ci.sh` 开始。不要把它们未隔离地直接全量运行。Host 用独立 `ION_HOST_SOCKET`，会话和配置使用本轮私有目录；结束只清理自己启动的 PID。

### 9.6 24 小时时间安排

| 相对时间 | 工作 | 继续条件 |
|---|---|---|
| 0–2h | 记录 SHA、环境、基线失败；建立唯一运行目录、锁、进程超时和结果清单，领取 T01 | 不影响用户环境；失败可向上传递 |
| 2–6h | T02–T03，复验错误注入与调度；冻结第一套可信测试集 | 测试只执行一次，真实失败使整轮失败 |
| 6–12h | 在 T04/T05/T06 中按复现影响选 1–2 项小修复；每项单独验证 | 每次改动有 Harness + RPC 证据；未完成就保存阻塞原因 |
| 12–22h | T08 连续耐久窗口，穿插已定位的小修复；修改后重新记录该版本的耐久起点 | 连续两轮同类基础设施失败停止修改；不盲目换题 |
| 22–24h | 冻结修改，最后回归，汇总 diff/失败/耗时/实际成本/后续队列 | 完成最终报告，暂停自动化 |

这里是「覆盖 24 小时的验证与修复窗口」，不是声称同一版本已经连续压测 24 小时。若要求后者，先冻结通过基线的提交，再另外开完整 24 小时耐久窗口；期间任何代码改动都重置该版本计时。

每小时唤醒一次、每批最长 40 分钟，其余时间用于退出、清理和留出余量。长任务必须在进程层有 timeout/终止处理，不能仅靠提示词。首次实际启用时记录固定截止时间；机器休眠或应用离线时不能保证按时执行，报告必须列出缺测窗口，不补造运行记录。

### 9.7 循环与交接协议

每轮固定为：读上轮证据 → 检查截止时间/预算/独占锁 → 选择一个任务 → 验证复现 → 小步修改 → Harness → RPC → 记录 → 释放锁。锁已占用则跳过，不另开一批；同一错误最多两次修复，连续两批基础设施失败则暂停。

代码只在专用 `codex/` 分支和独立工作副本修改。第一轮记录主工作区的干净/脏状态；不覆盖用户改动，不自动合并/推送/发布，不清理用户 `target/` 或常驻 host。旧脚本的自动合并开关不能代替这些约束。

每轮交接必须包含：run/attempt、任务 ID、开始与结束时间、基线和候选 SHA、文件列表、测试命令、退出码、有效断言数、耗时、日志位置、资源快照、成本或 unknown、失败分类、下一步。结果区分 PASS / FAIL / SKIP / TIMEOUT / NOT_RUN。

验证日志属于本次测试运行产物，可放独立运行目录；ION 会话的目标、预算、模型等派生状态仍只能使用 SessionIndex 或会话 JSONL，不通过报告文件反向维护权威状态。

### 9.8 已安排的自动化与启用条件

已创建 Codex 自动化 **ION 24 小时验证与小步修复（待选执行模型）**，ID `ion-24`，每小时检查，当前 **PAUSED**。它绑定当前任务，尚未开始计时，也没有切换当前模型。

用户可先切换此任务的执行模型再启用；若希望每轮都由明确指定的模型独立运行，应改为绑定 ION 项目的独立定时任务，并暂停本草案以免重复。当前未创建额外执行任务，也未启动 ION 的真实模型循环。

到期暂停目前是自动化提示词中的执行约定，尚未实现操作系统级硬截止。因此 T01–T03 的执行基础应包含进程超时与截止时间校验，通过后再宣称具备无人值守保护。只有发生新失败、完成、需用户处理或到期汇总时通知，无变化保持安静。

> **第 1 轮实测勘误（2026-09-08）**：当前 workspace 的自动化列表为空——`ion-24` 不在这里（可能建在另一 workspace 或已被删）。启用前必须先确认其归属；这条差异本身也说明「自动化提示词约定」不等于系统级保证。

### 9.9 第 1 轮执行记录（2026-09-08，T01+T02）

执行者：直接会话（非定时唤醒）。按 §9.7 交接字段记录：

| 字段 | 内容 |
|---|---|
| run / attempt | run-001；T01、T02 各 1 次修复通过（未用满两次配额） |
| 任务 ID | T01、T02 |
| 开始 / 结束 | 2026-09-07T18:37:33Z – 18:47Z（本地 2026-09-08 02:37–02:47） |
| 基线 SHA | `c0f2901`（工作区原有 round-0 文档改动，已作为 `e217f8d` 原样提交） |
| 候选 SHA | `219f7fd`（分支 `codex/ion24-t01-t02`，master 未动，未推送） |
| 文件列表 | `scripts/aggregate_ci_results.sh`（重写）、`scripts/run_ci_matrix_parallel.sh`（11 处定点修改）、`tests/aggregate_ci_fault_ci.sh`（新增）、`tests/ci_matrix_schedule_ci.sh`（新增）、本记录 |
| 测试命令 | `bash tests/aggregate_ci_fault_ci.sh`；`bash tests/ci_matrix_schedule_ci.sh`；`bash -n` ×4 |
| 退出码 | 全部 0（T01 套件 15/15 ×2 次；T02 套件 15/15 ×3 次） |
| 有效断言数 | 30（15+15，故障注入 + 假项目调度注入） |
| 耗时 | T01 套件 ~2s/次；T02 套件 ~12s/次（内含 3 轮假矩阵运行） |
| 日志位置 | `~/.ion/tmp/ion24/run-001/`（t01-repro/、t01-fault-injection.log、t02-schedule.log） |
| 资源快照 | 未启动 host/worker，零 cargo 编译，无进程残留；沙箱随 trap 自清 |
| 成本 | 0（未调用任何付费模型；全部分析修复在当前会话完成） |
| 失败分类 | 目标系统修复 0 次失败；测试脚本自伤 2 次（bash 3.2 多字节粘名、结果文件名笔误），当场修复 |
| 下一步 | T03（workflows + cargo shim 可信化）→ T04/T05/T06 择一；全量矩阵实跑放在 T03 之后用新 runner |

**T01 修复语义**：汇总器退出码 0=全 PASS/SKIP、1=任一 FAIL attempt、2=manifest 缺失/多出、3=畸形记录、4=无输入；attempts 全保留（报告含 FAIL→PASS 重试历史表），`all.jsonl` 不重复计数；无 manifest 时跳过缺失检查（兼容 `run_ci_matrix.sh` / `run_ci_matrix_rpc.sh` 旧调用方，默认路径 `/tmp/ci-results` 不变）。

**T02 修复语义**：删除 Phase 1 对全量 `FILTERED` 的第二次 xargs（并行脚本不再跑两遍、串行脚本不再跑三遍）；每运行独立 `RUN_ROOT=/tmp/ci-matrix-<ts>-<pid>`（bin/results/out/home/work）；源 `.ion/monitors` 不再 `rm -rf`（改为不触碰 + 提示）；输出 `manifest.txt` 供汇总器校验；runner 退出码 = 汇总器退出码（假成功链路切断）；worker id 由 `md5sum` 改为脚本 basename（去 GNU 依赖）；修复 `bn` 变量在 `.ion` 符号链接循环中被遮蔽的隐患。

**附带发现（下轮须知）**：

1. `ion-24` 不在当前 workspace（见 §9.8 勘误）——两处 cron 归属问题在启用前必须人工确认。
2. **bash 3.2.57 陷阱**：双引号内 `$var` 紧邻多字节字符（如全角括号）会把字节 ≥0x80 粘进变量名 → `unbound variable`。新测试脚本已全 ASCII；后续写 bash 时 `$var` 邻接非 ASCII 必须写成 `${var}`。
3. 串行 runner（`run_ci_matrix.sh` / `run_ci_matrix_rpc.sh`）现也继承汇总器的非零退出（行为升级，本轮未端到端实跑）；二者仍共享 `/tmp/ci-results` 且 rpc 版开头 `rm -rf` 它——同款隔离缺陷未修，建议并入 T03 或另开卡。
4. cargo shim 伪造 build/check/clippy/fmt/test 成功**未动**（T03 范围）；在 T03 完成前，矩阵报告中的 cargo 类 PASS 不可作为编译/测试证据。
5. 本机 `timeout`/`md5sum` 来自 `/usr/local/bin`（coreutils）；runner 已无 md5sum 依赖，`timeout` 仍必需。

### 9.10 当前检查点（连续执行·ion24-run-002）

> **本节是连续执行的权威检查点**，由主执行者在每个里程碑原位更新（开始任务前/长命令前/验证后/提交前后/切换任务前）。监督任务（每 10 分钟）只读本节与锁，不写入本节。**固定截止时间在任何重启、唤醒、上下文压缩后都不得重置。** 日志正文放 `~/.ion/tmp/ion24/run-002/`，本节只留结论与证据路径。本节是开发交接记录，不是 ION 产品会话状态存储；改 ION 会话派生状态仍遵守 SessionIndex/JSONL 落位原则。

- **运行标识**：ion24-run-002
- **工作副本**：`/Users/xuyingzhou/Project/study-rust/ion`，分支 `codex/ion24-t01-t02`，起点 SHA `1d519b4`（工作区 clean）
- **固定开始 / 截止**：2026-09-08 03:48:30 CST ／ **2026-09-09 03:48:30 CST（UTC+8 = 2026-09-08T19:48:30Z）——不可重置**
- **监督任务**：ZCode cron `automation-cf70ffcf-934d-46c5-8d83-51186290dd6f`（`*/10 * * * *`，enabled，停止 = CronDelete 该 ID）；唤醒留痕于 `~/.ion/tmp/ion24/run-002/supervisor-heartbeats.log`；不操作 Codex 侧 ion-24
- **锁与判活**：`~/.ion/tmp/ion24/lock/`（owner + heartbeat）；判活 = cmd_pid 存活 或 心跳 <30min（长命令按登记的 expected_done 判）
- **状态（常更）**：RUNNING — T03/T03b/T03c/T06/T07 已完成（8f8fbff/7746a0e），T05 实施中｜最后更新 2026-09-08 04:30 CST（UTC 20:30Z）
- **本轮已完成**：① 现场核验+T01/T02 复跑绿 ② **T03**（42df79d）workflows+诚实 shim+旧 runner 隔离，21/21×2 ③ **T06**（6ea28fd）索引隔离保全+get_index_health，5 场景+CLI 5/5+lib 991/0 ④ **T03b**（27ff9d8）clippy 19→0 ⑤ **T03c**（bed97f0）fmt 332 块→0 ⑥ **T07**（8f8fbff+7746a0e）README/DEPLOY_ARCH 删 ion-worker 入口、§8 归档标注、STATUS 基线换新（e9cb669：1169 提交/106,200 行/991 测试）。证据在 `run-002/`
- **当前任务（常更）**：**T05 Goal 中断恢复**——运行时缺口确认：`SharedGoalState` 在 worker_rpc.rs:344 初始化为 None，worker 重启即丢目标；goal-runs sidecar 属 evolver 脚本层（goal_evolver.rs 只读），非运行时状态
- **下一步动作（常更）**：T05 实施四步：(1) GoalSetTool/Refine 与每次 gate 迭代后 `session_jsonl::append_custom_entry` 落 `custom(goal_state)`（data=GoalState 全量，含 deadline/iterations/status）；(2) worker 启动时回放会话 JSONL 最后一条 goal_state 恢复 SharedGoalState（读法：FileIndex 或倒序扫描）；(3) SessionMeta 加 `goal_status: Option<String>`+`goal_deadline: Option<i64>`（四步 checklist：serde default→构造点→set 时顺便 upsert→existing 带回）；(4) 验证：单测（fixture JSONL→restore 断言）+ CLI 脚本（goal_set→kill worker→重连→goal_get 一致）+ lib 回归。⚠️ goal_evolver.rs 的 goal-runs 读取是脚本层设计，本卡不动它（另立卡评估 evolver 落位）
- **长命令登记（常更）**：（无进行中长命令）
- **未提交改动**：无
- **尝试次数/阻塞**：T03/T06/T07 均 0/2 配额一次通过；T05 第 1 次尝试（侦察已完成）。监督任务派发正常（runCount≥2）但被唤醒者未写心跳行——已知限制
- **本轮测试记录（常更）**：ci_trust_gates 21/21×2、session_index_fault 5 场景、CLI 5/5×2、lib 991/0×3、clippy 0、fmt 0（03:47–04:30 CST）
- **模型用量/费用**：unknown（会话内无法查询自身用量；未调用任何外部付费 LLM API；ION 侧全用脚本/FauxProvider）
