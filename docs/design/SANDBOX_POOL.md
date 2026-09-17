# SANDBOX_POOL 沙盒池 — 无状态远程执行端的统一管理

> **状态：Phase 1 已实现（2026-09-13，commit 0c7a26e）+ §3.3 审批停摆机制性解决已实现（2026-09-16，`sandbox_policy` RPC + 审批泵，`tests/sandbox_policy_ci.sh` 27/0）** — sandbox_pool 模块 + list_sandboxes / sandbox_probe / host=auto / sandbox_policy 四 RPC；五步试炼 `tests/sandbox_stateless_ci.sh` 8/0（真机，勿在开发机直跑——默认端点即生产 win38）。**K5 真机验证待办三件的零准备演练已就绪**（`tests/sandbox_live_readiness.sh` --check/--plan/--run 双确认 + `tests/sandbox_live_readiness_ci.sh` 42/0，见 §4.3 授权窗口演练手册）。§4.2 跨沙盒重派已被 fix4 覆盖（commit 8110919，`failover_decision` + `auto_recovered` 事件）；Phase 3 UI 面板待开工。目标：远程服务无状态、会话统一在 Mac 管理、任意沙盒即派即跑。

## 0. 一句话

把"手工填 remote_workers 配置 + 人肉记住哪个沙盒能用"升级为**沙盒池**：统一视图（`list_sandboxes`）、单点体检（`sandbox_probe`）、自动派发（`host="auto"`）、跨沙盒续跑（AUTO-RECOVERY 重派目标选择）。

## 1. 已验证的地基（本设计不重复造轮子）

| 地基 | 验证方式 |
|------|---------|
| 客户端模式 M1-M4.5（SSH spawn / 零 key 桥接 / 会话回流 / VerbGate） | REMOTE_WORKER.md，`tests/remote_worker_ci.sh` 13/0/1 |
| 异构端点（Linux 直连 :2222 / Windows :22 wrapper / 云跳板 ProxyJump） | 2026-09-12 三端点真机全通 |
| 无状态语义（权威副本在 Mac，远端副本可丢弃） | kill -9 实证：Mac 会话零丢、host 直读照常 |
| Windows wrapper stdin 保真修复 | commit 7b0e3a3（载荷命令替换，fd0 不被管道偷走） |
| 双沙盒并行开发 + Mac merge 闭环 | ion-web/ui + ion-web/recovery 两分支零冲突合并 |

**无状态的准确语义**：worker 双写会话（远端本地副本 + 流式回流 Mac）。权威副本在 Mac；远端副本随时可扔（重装/清理不影响任何东西）；沙盒切换时新沙盒从零开始，上下文由 Mac 侧 LLM 桥组装喂入。

## 2. 沙盒清单（当前）

| 名称 | 端点 | 归属 | 状态 |
|------|------|------|------|
| `win38` | root@192.168.0.38:2222（WSL sshd 直连） | config.json | 生产可用 |
| `win38-a` / `win38-b` | 同上 / sshuser@…38:22 + `wsl` wrapper | env 注入（试炼） | 可用 |
| `shanbox` | root@shanbox-jump（经 replay 云跳板 ProxyJump） | 待入 config | 可用（二进制已对齐） |
| `replay` | root@106.55.9.129:22（腾讯云本体） | — | **待用户拍板**（生产中继节点） |

## 3. Phase 1 — 沙盒池抽象（开发中）

### 3.1 `src/sandbox_pool.rs`（新模块）

```rust
pub struct SandboxStatus {
    pub name: String,
    pub dest: String,            // user@host:port（展示用）
    pub health: SandboxHealth,   // Unknown | Reachable | Unreachable | VersionMismatch
    pub version: Option<String>, // 远端 worker_bin --version
    pub worker_count: usize,     // 该沙盒上在跑的 worker 数
    pub last_probe: Option<u64>, // unix ts
}

pub struct SandboxPool { /* config.remote_workers + 运行时状态 */ }

impl SandboxPool {
    pub fn from_config(cfg: &Config) -> Self;
    pub async fn probe(&mut self, name: &str) -> SandboxStatus;   // ssh 可达 + 版本比对，5s 超时
    pub fn pick_healthy(&self, exclude: &[&str]) -> Option<&str>; // 健康+负载最低；全都不健康则 None
    pub fn statuses(&self) -> Vec<&SandboxStatus>;
}
```

设计约束：
- **纯数据 + 逻辑进模块，ssh 探测走 tokio::process**（单测 mock 状态机，不做真网络）
- 版本对齐判定沿用 M1 铁律：远端 `--version` 与 Manager 不一致 → `VersionMismatch`（可派但 UI 黄牌警告）
- 不改现有 create_worker 行为（向后兼容）

### 3.2 RPC 面

**`list_sandboxes`**（无参数）：

```json
{"success":true,"data":{"sandboxes":[
  {"name":"win38","dest":"root@192.168.0.38:2222","health":"Reachable","version":"0.4.1","workerCount":1,"lastProbe":1789300000}
]}}
```

**`sandbox_probe {name}`**（主动体检，返回更新后的单条 status；probe 顺带刷新缓存）。

**`create_worker` 扩展 `host:"auto"`**：Manager 调 `pick_healthy()`；池空/全不健康 → 报错 `no healthy sandbox`。手工指定 host 的路径原样保留。

### 3.3 提示词三层模型（为什么任务卡越来越短）

| 层 | 内容 | 承载机制 |
|----|------|---------|
| 环境层 | 工具链/网络/PATH/版本/**审批策略** | **沙盒档案**（probe 产出），派发时自动注入 initial_prompt 前缀。教训实例：WSL 的 cargo 在 `~/.cargo/bin` 不在非交互 PATH——修法是 `ln -sf` 进 `/usr/local/bin`（修沙盒），不是往任务卡里写"用全路径"（改提示词） |
| 规范层 | AGENTS.md 项目惯例 | clone 内自动生效（路径命名空间=执行侧），零成本 |
| 任务层 | 四要素：目标 / 验收标准（可自验的命令）/ 文件约束（防 merge 冲突）/ 交付格式 | 任务卡人写，这是唯一需要动脑的部分 |

**🔴 审批停摆教训（2026-09-13，worker B 实录）**：无人值守的沙盒 dev worker 写文件会触发 `file-approval` 扩展的 `ApprovalRequest`，agent 停下等批准——协调方没盯审批队列 → 4 分钟后 worker 超时退场（表现酷似"连接不稳"，host 日志里 agent_end + ApprovalRequest 才是真相）。**先查日志再归因**。解法按优先级：① 沙盒档案增加 `approval_policy: auto_approve` 字段（✅ **已实现 2026-09-16**：`remote_workers.<name>.approval_policy` / `.notes` 配置 + `sandbox_policy` RPC 运行时覆盖 + host 侧审批泵——reader loop 捕获 `ApprovalRequest` → 短锁判定（策略 + 2s 冷却）→ 锁外 `review_approve_all` + 广播 `SandboxAutoApproved` 事件，详见 §3.4）；② 协调方挂审批泵（轮询 `review_pending` → `review_approve_all`，当前 Phase 1 已用）；③ 人盯队列（不可持续，弃）。

### 3.4 `sandbox_policy` RPC + 沙盒档案（§3.3 ①的实现）

**配置面**（`remote_workers.<name>` 新增两字段，均缺省安全）：

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `approval_policy` | string | `""`(=default) | `auto_approve` = 审批泵自动放行；其他值宽容回落 default |
| `notes` | string[] | `[]` | 环境层事实（如 "cargo 在 ~/.cargo/bin"），派发时注入 initial_prompt 前缀（先环境层后任务层） |

**查询（GET）：**
```bash
ion rpc --method sandbox_policy --params '{"host":"win38"}'     # host 级
ion rpc --session <sid> --method sandbox_policy --params '{"worker":"<wid>"}'  # worker 级
```
成功响应（GET host）：`{"success":true,"data":{"scope":"host","key":"win38","profile":"auto_approve","effective":"auto_approve","override":null}}`
（GET worker：`{"scope":"worker","key":...,"effective":...,"override":...}`）

**设置（SET）：**
```bash
ion rpc --method sandbox_policy --params '{"host":"win38","policy":"auto_approve"}'
```
成功响应（SET）：`{"success":true,"data":{"scope":"host","key":"win38","policy":"auto_approve","effective":"auto_approve"}}`
失败响应：`{"success":false,"error":"unknown policy 'yolo' (expected auto_approve | default)"}`

**验证点：**
- ✅ 生效链优先级：worker 覆盖 > host 覆盖 > 出生档案（record.approval_policy），写后回读三层一致
- ✅ `list_sandboxes` 每条附带 `approvalPolicy`（生效值）+ `notes`
- ✅ SET 走严格校验（非法值/未知沙盒明确报错，不静默回落）；GET/SET 均广播 `SandboxPolicyChanged` 事件
- ✅ 泵只在 `auto_approve` 生效时 fire；2s 冷却窗防 gate check 重复触发刷屏
- ✅ 覆盖为内存态（host 重启即失，符合"宁可丢也不建新文件"存储原则）
- ✅ 对照组：default 策略同样任务 → 无 `SandboxAutoApproved`、`review_pending` 保留（人工审批语义不变）
- ✅ CLI：`bash tests/sandbox_policy_ci.sh`（mock 级零 ssh，27 断言）

## 4. Phase 2 — 无状态闭环 + 跨沙盒续跑

### 4.1 `tests/sandbox_stateless_ci.sh`（参数化五步试炼）

```
环境变量: ION_BIN / RW_HOST / RW_PORT / RW_USER / RW_WRAPPER（端点不可达整组 SKIP）
① env 注入单端点，起隔离 host（私有 socket，学 remote_worker_ci.sh 三件套）
② create_worker 真任务（远程 bash 写文件+回报内容）
③ ssh 到远端 kill -9 worker（精确 pkill 模式，禁宽泛 pkill ion——见 AGENTS.md 红线）
④ 断言 Mac 会话 JSONL 存在且含 ToolResult + 最终 Assistant 回答（数据零丢）
⑤ host 直读 get_session_messages 成功（worker 已死仍可读）
```

任何新沙盒接入跑一遍即认证入库。

### 4.2 AUTO-RECOVERY 重派目标选择（✅ 已被 fix4 覆盖，commit 8110919）

原设计的增强点已随修复批 4 落地，无需重做：
- 重派前按池决策换沙盒：`sandbox_pool::failover_decision(pool, original_host)`——非池成员/显式指定原样回原沙盒；池成员死 → `pick_healthy(exclude=[原沙盒])` 换节点；全不健康 → 回落原沙盒 + WARN（respawn 决策区，`src/worker_registry.rs`）
- 事件 `auto_recovered` 带 failover 三态 mode（`pool_pick` 原沙盒仍健康选回自身 / `pool_failover` 换节点 / `fallback_no_healthy` 全不健康回落），UI 可见
- 全部不健康时保持 Dead 保留语义，等 `recover_tree` 手动触发
- 验证：`src/sandbox_pool.rs` 单测 failover 三态 5 条 + `tests/heartbeat_ci.sh`

### 4.3 授权窗口演练手册（K5 真机验证待办三件的零准备执行）

> **状态：就绪脚本已完成（2026-09-17）**——三套件的执行体全部就位并经 mock CI 验证（`tests/sandbox_live_readiness_ci.sh` 36/0），只等授权窗口一键真跑。

Phase 1/2 落地后有三件**只能真机做**的验证一直欠着。环境准备成本已压缩到零：

| # | 套件 | 验证什么 | 执行体 | 预计时长 |
|---|------|---------|--------|---------|
| ① | `stateless` | 无状态五步试炼（§4.1）：真任务→ssh kill -9→会话回流 Mac→host 直读 | `tests/sandbox_stateless_ci.sh` | 2-4 分钟 |
| ② | `pump-e2e` | 真沙盒审批泵（§3.3）：auto_approve 沙盒 worker 真实写文件→ApprovalRequest→泵自动放行→`SandboxAutoApproved`+`review_pending` 归零 | `tests/sandbox_live_drill.sh pump-e2e` | 2-4 分钟 |
| ③ | `policy-rpc` | `sandbox_policy` RPC 真机设置（§3.4）：list_sandboxes/sandbox_probe 真 ssh 体检（可达+版本）/GET→SET→回读→非法拒绝 | `tests/sandbox_live_drill.sh policy-rpc` | ~30 秒 |

**用法（推荐路径）：**

```bash
bash tests/sandbox_live_readiness.sh --check          # 平时摸底：零副作用（不 ssh/不写盘/不读 auth.json/不打印密钥）
bash tests/sandbox_live_readiness.sh --plan           # 给人看：三套件步骤/影响面/时长

# 授权窗口到：
cargo build --bin ion                                 # 或 ION_BIN 指向已构建产物
export SANDBOX_LIVE_CONFIRM=YES                       # 第一重确认
bash tests/sandbox_live_readiness.sh --run all        # 第二重确认：终端输入 yes；①→②→③ 依次执行
# 或单套件: --run stateless / --run pump-e2e / --run policy-rpc
```

**换非生产端点**（缺省端点=生产 win38，脚本会🔴提示）：

```bash
RW_HOST=<host> RW_PORT=<port> RW_USER=<user> [RW_KEY=~/.ssh/xxx] [RW_WRAPPER='wsl -d ion -u root'] \
  bash tests/sandbox_live_readiness.sh --check        # 先 --check 探测新端点
RW_HOST=<host> ... bash tests/sandbox_live_readiness.sh --run policy-rpc   # RW_* 原样透传给执行体
```

**`--check` 就绪判定项**（每套件独立结论"可执行/缺什么"）：ion 二进制已构建、nc/ssh/python3/jq 齐备、端点 TCP 可达（nc 端口探测，绝不 ssh）、LLM 配置就绪（①②需要；③隔离 HOME 不需要）、`extensions.file-snapshot.enabled=true`（②需要，否则写文件不触发 ApprovalRequest）、RW_KEY 文件存在。

**安全门（缺一即拒，exit 2）**：`SANDBOX_LIVE_CONFIRM=YES` + 终端交互输入 yes；非交互环境（CI/脚本管道）一律拒绝——防误触真机。端点 TCP 不可达在确认前即拒（exit 3）。缺确认时只打印"将要做的事"，绝不执行。

**影响面**（授权窗口内已知且可控的副作用）：①②各产生 1 个真实 LLM 会话（本机 JSONL 正常落盘）+ 远端 /tmp 1 个时间戳文件；①的 ssh 仅精确 kill 本套件自己的 worker；③零持久化（隔离 HOME 用完即删，远端仅只读 `--version`）。

**已验证**：`bash tests/sandbox_live_readiness_ci.sh`（42 断言：--plan 三套件可见/--check 零副作用+清单+缺项判定/--run 四种拒绝路径/三执行体不可达整组 SKIP 回归/Phase F fake-ion 全链 mock——policy-rpc 的 15 项真机断言逻辑在 mock 下全数走通，真机窗口只剩环境变量差异）。端点参数化补齐：`sandbox_stateless_ci.sh` 新增 `ION_SESSION_DIR` 支持（host 与 ④回流断言同步切换，演练可整目录隔离会话落盘）；RW_HOST/RW_PORT/RW_USER/RW_KEY/RW_BIN/RW_NAME/RW_WRAPPER/ION_BIN 原已支持。

## 5. Phase 3 — ion-web 沙盒管理面板

- 沙盒卡片墙：`list_sandboxes` 数据（状态/版本/在跑会话/一键 probe）
- 派发交互：沙盒选择（含 auto）→ 任务输入（环境层自动注入，用户只写任务层四要素）→ 实时事件流
- 中断复原面板：`list_interrupted` 会话树 → 一键 `recover_tree`（消费 ION_WEB_RECOVERY_RPC.md 设计）
- 开发过程本身用双沙盒（A 内核 / B UI），dogfooding

## 6. Phase 4 — replay 云端接入 + 终局验收（待用户拍板）

- replay 部署 ELF（x86_64 同款）+ 入池
- **终局演示**：四沙盒卡片 → 随手派任务 → 跑一半杀掉 → 一键恢复到另一沙盒续跑完成 → 全程 Mac 零脚本执行、真 key 不出本机

## 7. CLI 验证（Phase 1 完成标准）

```bash
# Group A: 池视图
ion rpc --method list_sandboxes
# ✅ 返回全部 config+env 注入的沙盒，health/version/workerCount 齐全

# Group B: 体检
ion rpc --method sandbox_probe --params '{"name":"win38"}'
# ✅ Reachable + 版本一致
ion rpc --method sandbox_probe --params '{"name":"nope"}'
# ✅ 明确报错 unknown sandbox

# Group C: auto 派发
ion rpc --method create_worker --params '{"host":"auto","agent":"build","initial_prompt":"ping","wait":false}'
# ✅ 落在健康沙盒上；list_workers 的 host 字段 = 被选中的沙盒名

# Group D: 全不健康
# （拔网线/改错全部 hostname 后）host=auto → "no healthy sandbox"
```

## 8. 相关

- [REMOTE_WORKER.md](./REMOTE_WORKER.md) — 客户端模式本体（M1-M4.5）
- [ION_WEB_RECOVERY_RPC.md](./ION_WEB_RECOVERY_RPC.md) — list_interrupted / recover_tree 设计
- [AUTO_RECOVERY.md](./AUTO_RECOVERY.md) — 断点续作现状
