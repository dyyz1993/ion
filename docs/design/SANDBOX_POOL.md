# SANDBOX_POOL 沙盒池 — 无状态远程执行端的统一管理

> **状态：开发中** — Phase 1-2 开工（2026-09-13）。目标：远程服务无状态、会话统一在 Mac 管理、任意沙盒即派即跑。

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

**🔴 审批停摆教训（2026-09-13，worker B 实录）**：无人值守的沙盒 dev worker 写文件会触发 `file-approval` 扩展的 `ApprovalRequest`，agent 停下等批准——协调方没盯审批队列 → 4 分钟后 worker 超时退场（表现酷似"连接不稳"，host 日志里 agent_end + ApprovalRequest 才是真相）。**先查日志再归因**。解法按优先级：① 沙盒档案增加 `approval_policy: auto_approve` 字段（Phase 1.5，从机制上解决）；② 协调方挂审批泵（轮询 `review_pending` → `review_approve_all`，当前 Phase 1 已用）；③ 人盯队列（不可持续，弃）。

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

### 4.2 AUTO-RECOVERY 重派目标选择

现状：worker 死后原沙盒重派。增强：
- 重派前 probe 原沙盒：不可达 / VersionMismatch → `pick_healthy(exclude=[原沙盒])` 换沙盒续跑
- 事件 `autorecovery_reassigned` 带 `{from, to, reason}`，UI 可见
- 全部不健康 → 保持现有 Dead 保留语义，等 `recover_tree` 手动触发

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
