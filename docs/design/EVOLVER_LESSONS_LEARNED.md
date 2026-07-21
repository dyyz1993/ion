# Evolver 经验文档 — 问题与解决方案

> **状态：持续更新** — 每次 A→B 自进化过程中遇到的问题和解决方案。

## 1. LLM 不走 container B，在 host 上改代码

### 现象
A（evolver agent）被要求"通过 container exec B 改代码"，但实际上 LLM 直接在 host 上用 `read`/`grep`/`sed`/`python3`/`cat >` 改代码。

### 根因
- glm-5.2 倾向于"先理解再动手"，不听 prompt 里"不要分析直接执行"的指令
- edit/write 工具被禁了，但 LLM 会找替代方式（sed/cat/python3）

### 解决方案
CommandGuard 拦截所有 host 代码修改路径（已实现）：
- `sed -i` → High Deny
- `python3 -c` → High Deny  
- `python -c` → High Deny
- `cat >` → High Deny
- `./target/release/ion --agent` → High Deny
- `./target/debug/ion --agent` → High Deny

### 仍存在的问题
LLM 被拦后卡住（不知道走 container），不会自动切换到 container exec B 路径。
更强模型（claude）可能更听话。

---

## 2. bash_run timeout 1800s 杀进程

### 现象
evolve.sh 编译 ion 需要 10-15 分钟，bash_run 前台模式 timeout=30s（默认），即使设了 ION_BASH_RUN_TIMEOUT=1800 也会被 agent_loop 外层 timeout 杀。

### 根因
两层 timeout 叠加：
1. bash_run 内部 timeout（默认 30s，可设 ION_BASH_RUN_TIMEOUT）
2. agent_loop 外层 timeout（默认 1800s，可设 ION_TOOL_TIMEOUT）

### 解决方案
- bash_run timeout 改成读 `ION_BASH_RUN_TIMEOUT` 环境变量（src/agent/bash.rs）
- agent_loop 外层 timeout 读 `ION_TOOL_TIMEOUT`（默认 1800s）
- 用 `background=true` + `timeoutBackground=true` 让进程转后台
- 后台进程完成时 spawn_watcher 自动发 follow_up（src/agent/bash.rs:369）

### 仍存在的问题
LLM 不用 `background=true`，总是用同步 bash_run。

---

## 3. auto_continue 导致 300+ 次轮询

### 现象
A 调 bash_run(background=true) 后，auto_continue 每轮注入 "继续执行" follow_up。LLM 每收到 follow_up 就跑一轮，忍不住去检查编译状态（container exec cat /tmp/ion-build-done）。导致 300+ 次无意义的 bash 调用。

### 根因
auto_continue 在 follow_up_queue 空时无条件注入 follow_up。对 evolver agent，这导致 LLM 每轮都"忍不住"检查状态。

### 解决方案
- evolver agent 不启用 auto_continue（src/bin/ion_worker.rs）
- 用 ION_WAIT_BACKGROUND：outer_loop 等 5s 检查异步 follow_up（src/agent/agent_loop.rs）
- bash_run background 进程完成时 spawn_watcher 发 follow_up → outer_loop 消费

### 仍存在的问题
follow_up_rx 在 agent loop 主循环的 try_recv 消费。如果 inner_loop 正在等 LLM 响应，follow_up 堆积在 channel。ION_WAIT_BACKGROUND 解决了 outer_loop 退出问题，但如果 LLM 在 inner_loop 里卡住（等 API 响应），follow_up 不被消费。

---

## 4. ~/.ion 只读挂载导致 B 的 session 丢失

### 现象
B 在 container 里跑 ion --agent developer，session 写到 container 的 /root/.ion/agent/sessions/。但 ~/.ion 挂载是只读（:ro），container 停了 B 的 session 就没了。导出 HTML 只有 A 的 6 条 entries。

### 根因
evolve.sh 用 `-v ~/.ion:/root/.ion:ro` 只读挂载。

### 解决方案
改成 `-v ~/.ion:/root/.ion`（可写）。B 的 session 写到 host 的 ~/.ion/agent/sessions/，container 停了也能导出。

---

## 5. worktree 的 .git 在 container 里无效

### 现象
git worktree 的 .git 是一个文件（指向主仓库的 gitdir），container 里路径不存在，导致 container 里所有 git 命令失败。

### 根因
worktree 的 .git 文件内容：`gitdir: /Users/xxx/ion/.git/worktrees/xxx`。container 不知道这个 host 路径。

### 解决方案
evolve.sh 在 worktree 创建后，把 .git 从文件转成独立 git repo（git init + add + commit）。worktree 变成独立仓库，container 里 git 可用。

---

## 6. Cargo.toml 路径在 container 里不对

### 现象
Cargo.toml 写的是 `path = "../ion-provider"`，container 里 `/workspace/../ion-provider` 不存在。编译失败。

### 根因
ion-provider 在 host 的 `/Users/xxx/ion-provider`，container 里没有这个路径。

### 解决方案
evolve.sh 挂载 ion-provider 到 container 的 `/ion-provider`，然后 sed 改 Cargo.toml：`path = "../ion-provider"` → `path = "/ion-provider"`。

### 仍存在的问题
同步代码到主仓库时 Cargo.toml 的路径是 `/ion-provider`（container 路径），需要手工改回来。evolve-run.sh 的同步逻辑跳过 Cargo.toml。

---

## 7. host idle 检测杀 worker

### 现象
A 在等 bash_run（evolve.sh 编译 15 分钟）时，host 的 idle 检测认为 A 完成了，杀掉 A。

### 根因
worker 创建时 status=Idle。bash 工具执行期间 worker 不改 status。idle 检测看到 Idle + 宽限期过了就杀。

### 解决方案
- worker 创建时设 Busy（src/worker_registry.rs）
- idle 宽限期默认 1800s（src/bin/ion.rs）

---

## 8. ZCode bash 工具 10 分钟 timeout

### 现象
通过 ZCode 跑 `ion --host --agent evolver` 时，ZCode 的 bash 工具 10 分钟 timeout 杀了整个进程树。

### 根因
ZCode 的 bash 工具有固定 10 分钟 timeout。

### 解决方案
用 `run_in_background: true` 绕过。或直接在终端里跑（没有 timeout 限制）。

---

## 9. container 里编译太慢

### 现象
container 里 cargo build --release 需要 10-15 分钟（首次编译）。

### 根因
Apple Container 是 Linux VM，没有编译缓存。每次从头编译所有依赖。

### 解决方案（未实现）
- Dockerfile.evolve 多阶段构建：stage 1 编译 ion，stage 2 copy binary
- 或 container image 预装 ion binary
- 或 mount host 的 target/ 目录（跨架构不兼容，不行）

---

## 10. B 改的代码不只限于目标文件

### 现象
B（developer agent）有 read/edit/bash 工具，可能改了 src/ 下多个文件，不只是 global_memory.rs。同步时全量 rsync src/ 导致其他文件被覆盖。

### 根因
B 是独立的 developer agent，它自由决定改哪些文件。

### 解决方案
evolve-run.sh 用 diff 比较 worktree 和主仓库的 .rs 文件，只同步有改动的文件（不用 rsync 全量覆盖）。

---

## 经验总结

### 做得对的
1. A→B 架构正确（A 不碰代码，B 在 container 改）
2. CommandGuard 拦截有效（堵住了所有 host 绕过路径）
3. bash_run background + follow_up 机制设计正确
4. worktree bind-mount 让 B 的 commit 实时同步到 host

### 需要继续做的
1. LLM 行为调优（让 glm-5.2 更听话走 container）
2. 看门狗实现（A 安全重启加载新代码）
3. reviewer agent（审查 B 的代码）
4. 回滚机制（合并后最终验证失败自动 revert）
5. container image 预装 ion（跳过编译）
6. 完整 CI 套件（所有 tests/*_ci.sh）
