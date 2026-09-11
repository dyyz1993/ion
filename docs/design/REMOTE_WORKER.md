# REMOTE_WORKER 远程执行端设计文档

> **状态：M1-M4 全部完成（2026-09-11：M1 SSH spawn ✅；M2 零 key 桥接 "42" ✅；M3 会话回流+资产包+hooks 禁用 ✅；M4 VerbGate 动词表真机验证 ✅——授权读取/越权拒绝/审计留痕三断言全过），commit 54cee34+44895e2。** 设计经与用户多轮收敛 + 红蓝对抗安全评审。本文档是客户端模式的唯一设计入口；服务端模式已上线（win38）。

### M1 实现补充（2026-09-10）

- `RemoteWorkerHost` 实际字段比原设计多两个：`wrapper`（Windows+WSL 执行端的前缀，如 `wsl -d ion --`，此模式下脚本走 **base64 转运**免疫 cmd/bash 双层引号）与 `env`（连接级环境变量，如代理指回主控端）
- **win38 实际走"WSL 原生 sshd + Windows portproxy 2222 直连"**（wrapper 留作通用 Windows 执行端能力）：实测发现 `ssh → cmd.exe → wsl.exe` 链路的 stdin 中继**首次写入即 EOF**（远程 worker 收到 Manager 首个回复就优雅退出），WSL 内装 sshd + `netsh portproxy` 直连 Linux 后 stdio 全程原生。portproxy 由开机任务 `ion-wsl-gateway` 自动刷新（WSL IP 动态）
- 验证：lib 1003/0 + 单测 5（含真实 /bin/sh 转义往返闸门）+ `tests/remote_worker_ci.sh` 6/6（faux 零 LLM 成本）+ 两轮重启零干预验收 + **真 LLM 端到端 "24"**（2026-09-11 版本对齐后）
- 🔴 **版本对齐铁律（实测教训）**：remote_workers 的 `worker_bin` 必须与 Manager **同源同版本**——远端旧版（0.4.0）worker 与新版 Manager 的 spawn 协议存在 provider 构造差异，表现为 worker 必 401 CreditsError 而同机直跑 `-p` 正常（四格对照锁定）。win38 现已部署 WSL 本地编译的同版本二进制（旧版备份 `ion-0.4.0.bak`）；后续版本升级需同步重编远端或经资产包分发（M3）

---

## 概览

把局域网里的第二台机器（win38，Windows 10 + WSL2 Ubuntu，已有 ion 二进制与免密 SSH）变成 ION 的**远程执行端**：Mac 是主控端（大脑、UI、审批、唯一真 key 所在地），远端是肌肉（执行代码、跑构建、消化不可信任务）。

**两种模式，一个二进制：**

| | ① 客户端模式（本文档，待建） | ② 服务端模式（已上线） |
|---|---|---|
| 本质 | 远程只是执行器：接指令、跑代码、回结果 | 远程是完整个体：全套部署，自带 key 与会话 |
| Key | **零 key**（LLM 经主控端桥接，连假的都没有） | 远程持有（建议专用低额度 key 或回连 Mac 网关） |
| 会话 JSONL | **流回 Mac 落盘**（远端无状态） | 留在远端自己的 `~/.ion/agent/sessions/` |
| skills/hooks 等资产 | **主控端打包下发**（远端项目级策略默认不加载） | 远端自管 |
| 市面同构 | GitLab Runner / Bazel REAPI / K8s ConfigMap / Anthropic 代码沙箱 | 自托管 n8n / JupyterHub / LangGraph Platform self-hosted |
| 适用 | 不可信任务沙箱、一次性重活（编译/转码）、Mac 会话驱动的委派 | 无人值守自动化、独立算力节点 |
| ION 现状 | 🔧 本文档 | ✅ win38 `ion serve` + Mac 侧 `ion38` 快捷命令 |

| 能力 | 入口 | 状态 |
|------|------|------|
| 远程 worker 拉起（SSH spawn） | `spawn_worker` 工具 + `remote_workers` 配置 | 🔧 设计稿 |
| LLM 桥接（零 key） | worker 协议 `llm_request/llm_chunk/llm_done/llm_error/llm_cancel` | 🔧 设计稿 |
| 会话回流（远端无状态） | worker → Manager `session_entry` 消息 | 🔧 设计稿 |
| 资产包下发 | spawn 时 rsync `--assets-dir` | 🔧 设计稿 |
| 通道化宿主访问（动词表） | worker → Manager `host_call`（封闭动词） | 🔧 设计稿 |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| M1 | SSH spawn 适配器 | ✅ | `tests/remote_worker_ci.sh` Group A 6/6 + 重启零干预 ×2 |
| M2 | LLM 桥接全链路 | ✅ | lib 1007/0（3 桥接单测）+ 真机零 key "42"（usage 9398 回传）|
| M3 | 会话回流 + 资产包 | ✅ | 真机：Mac `<sid>.jsonl` + UI 直读对话；远端 skills 部署实证 |
| M4 | 动词表（fs/http） | ✅ | 真机：授权读 Cargo.toml + 越权拒 ~/.ssh//etc/passwd + 审计 host_call 条目 |

### M2 实现补充（2026-09-11）

- 线协议简化：M→W 只需 `llm_chunk`（每帧带完整 StreamEvent serde；Done/Error 终帧本身携带最终消息/usage，worker 侧据此完成流，无需独立 done/error 消息）；llm_cancel 保留（W→M，abort 时关上游 SSE）
- 🔴 **worker stdin 双分发点**：主循环 match（idle）与 agent.run 内部 select 的 in-run 分发器（busy）——新命令两个分发点都要挂臂（llm_chunk 起初只挂主循环，全部流式分片被 in-run 分发器吞成未知命令，agent 120s 空闲超时）
- manager 写 worker stdin 的 Send 约束：spawn 任务里 parking_lot guard 不能跨 await——`write_line_to_worker_sync`（poll_write + Waker::noop 同步轮询，管道满 2ms 重试 5s 上限）
- bridge 优先于 faux（worker 侧同时设置时）→ CI/harness 可确定性测桥接（host 带 ION_FAUX_REPLY+ION_PROVIDER_BRIDGE=1：worker 走桥接、manager 侧桥接 registry 也注册 faux）
- 配置：`remote_workers.<name>.llm_bridge: true`（默认 false=M1 过渡形态远端自带 key）；spawn 注入 ION_PROVIDER_BRIDGE=1

### M3 实现补充（2026-09-11）

- **会话回流=双写镜像**：worker 照写本地 + 每条 header/entry 镜像给 Manager 落 Mac `<hash-dir>/<sid>.jsonl`（断线远端有副本，可靠性优先）。🔴 主消息持久化在 `save_worker_session`（整轮结束直写文件）而非 append_raw_entry——mirror 必须双挂点
- **资产包 v1**：spawn 前 tar 管道部署 Mac skills/agents 到远端 `~/.ion/agent/`（worker 加载零改动）；`assets:{skills 白名单,agents}`；失败降级
- **远端项目级 hooks 禁用**：ION_NO_PROJECT_HOOKS=1（llm_bridge 注入）→ hooks loader 跳过项目级——hooks 供应链 P0 的执行端克星
- **WSL 执行端可靠性三件套**：serve 保活 VM（VM 空闲停机会灭 sshd/IP）+ `refresh` spawn 前自愈（经 22 管理通道跑 gateway：起 sshd+刷 portproxy）+ `refresh_dest` 双通道凭据（22=Windows/sshuser vs 2222=WSL/root）
### M4 实现补充（2026-09-11）

- worker 侧工具：host_read/host_write/host_fetch（host_tools.rs，仅 bridge 模式注册，持 ManagerBridge 句柄同步往返）；经 ManagerBridge `_reply_to` 机制天然同步
- Manager 侧：verb_gate_execute（grants 默认全拒 → **canonicalize 防路径穿越**（实测 /etc/passwd → /private/etc/passwd 后比对）→ 限额执行 → 审计）。审批 UI 集成留 v2（开放问题 2）
- 🔴 from_worker 双查坑在 verb_gate 与审计两处重犯（session_id↔worker_id）——新增 Manager 命令处理时 from_worker 一律双查
- grants 语义：`/prefix/**` 前缀匹配 + 精确匹配；纯 glob 会命中 `../` 字符串（单测诚实断言），真实防线是 canonicalize
- 🔴 二进制特性自检必须行为测试（grep 字符串假阴性：LLVM 拆分字面量）；远端构建脚本 ~/ion-remote-tools/rw-build.sh 固化（tar 源码→touch 强制重编→行为自检→install）

---

## 0. 背景与决策记录（为什么这样设计）

> 这一节是讨论收敛过程的决策快照，每条都经过源码查证或真机实测。

| # | 决策 | 理由（一句话） |
|---|------|---------------|
| D1 | 双模式并存而非二选一 | 市面标准双轨（GitLab server+runner / Bazel）；两种用途互补 |
| D2 | 客户端模式 = **整个 worker 搬到远端**，不是副作用路由 | 现有 remote backend（runtime.rs:1508）只路由工具，bash 后台/LSP/Monitor/spawn_worker 四处绕层留在本地（实测 bash.rs:289 / lsp_extension.rs:452），隔离不完整 |
| D3 | **零 key 桥接**：worker 不持有任何 key，LLM 请求经 stdio 管道由 Manager 代发 | key 永不出 Mac；统一记账/限额/录制；沙箱被打穿也偷不到钥匙（红蓝对抗评审通过） |
| D4 | worker 只能报 model id，**不能指定 provider/base_url** | 否则被攻陷的沙箱可借 Manager 之手把上下文导向任意收集器（SSRF 式外泄）；model 走 Manager 侧白名单 |
| D5 | 会话条目**流回 Mac 落 JSONL** | 市面实践一致（CI 日志回中心/Bazel 结果回 CAS）；Mac UI 现有 host 直读路径零改动；远端真正无状态 |
| D6 | skills/hooks/agents/rules 以**资产包**形式 spawn 时下发 | 执行端无状态（K8s ConfigMap 同构）；远端项目级 `.ion/hooks.json` 默认**不加载**——同时根治 hooks 供应链 P0 |
| D7 | **路径命名空间 = 执行侧** | worker 读自己的盘；资产包禁止 Mac 绝对路径（便携性纪律，CI hermetic 同款）；跨机无裸路径可达正是隔离的一部分 |
| D8 | 跨机访问全部**通道化**（能力模型）：封闭动词表 + 策略→审批→审计→限额流水线 | "不是摸不到 Mac，是只能经命名通道摸"（用户决策）；MCP consent / K8s apiserver / 对象能力模型同构 |
| D9 | **永不提供 `host.bash` 类动词** | 有它则一切防线失效 |
| D10 | v1 用 SSH 按需拉起，无常驻守护 | 复用现成免密；机器坏 = worker 死（现有死亡检测直接映射）；多机穿 NAT 再演进为执行端主动外连 |

---

## 1. 配置

**文件**：`src/config.rs`（新增 `RemoteWorkersConfig`）+ `~/.ion/config.json`

```jsonc
{
  "remote_workers": {
    "win38": {
      "user": "sshuser",
      "hostname": "win38",            // 走 ~/.ssh/config 别名或 IP
      "port": 22,
      "worker_bin": "/usr/local/bin/ion",
      "cwd": "/root/workspace",        // 任务工作目录（远端路径）
      "assets": {                      // 资产包策略（D6）
        "skills": ["selected"],        // 白名单子集；缺省 = 全局 skills
        "hooks": false,                // 远端任务默认不带 hooks
        "agents": true,
        "rules": true
      },
      "grants": {                      // 动词授权（D8）：默认全拒
        "fs_read":  [],                // 路径 glob，如 ["/Users/me/proj/**"]
        "fs_write": [],
        "http_fetch": []               // 域名白名单
      },
      "llm": { "model_whitelist": ["zai/glm-5.2"], "max_tokens": 1000000 }
    }
  }
}
```

默认值：`remote_workers` 为空 → 行为与现状完全一致（零侵入）。

---

## 2. 总体架构与主流程

### 2.1 架构图

```
        Mac 主控端（Manager）                          win38 执行端（无状态）
┌────────────────────────────────────┐      ┌──────────────────────────────────┐
│ ion host（常驻）                     │      │ ion 二进制（预装）+ sshd          │
│  ├─ 你的会话 / UI / 审批 / 订阅      │ SSH  │                                  │
│  ├─ WorkerRegistry                 │─────▶│ spawn: ion --mode rpc            │
│  │   spawn_worker(host:"win38") →  │ stdio │   ├─ 完整 agent 循环（大脑）     │
│  │   ssh win38 ion --mode rpc      │ JSONL │   ├─ hooks/LSP/后台进程（远端）  │
│  ├─ ProviderBridge（真 key 在这）   │◀─────▶│   ├─ BridgeProvider（零 key）   │
│  ├─ VerbGate（动词表+策略+审计）     │      │   ├─ 会话条目 → 流回 Mac        │
│  └─ SessionIndex / JSONL（落 Mac）  │      │   └─ win38 文件系统 / GPU        │
└────────────────────────────────────┘      └──────────────────────────────────┘
  跨机面 = 封闭动词表：llm.complete / fs.read / fs.write / http.fetch（+ 会话/事件回流）
```

### 2.2 spawn 主流程

**文件**：`src/worker_registry.rs`（spawn 路径加分支）

```rust
// 现状：Command::new(current_exe()).arg("--mode rpc")  —— 本地拉起
// 新增分支：params.host 命中 remote_workers 时
//   1. 资产包：rsync skills/agents/rules 白名单 → win38:/tmp/ion-assets-<sid>/
//   2. 拉起：Command::new("ssh").args(["win38",
//        "ion --mode rpc --assets-dir /tmp/ion-assets-<sid> --provider bridge"])
//   3. stdio 管道处理与本地 worker 完全一致（JSONL 双向）
//   4. SSH 断线 = worker 死亡（复用现有死亡检测/Dead 保留逻辑）
```

### 2.3 LLM 桥接流程

**文件**：`ion-provider/src/bridge.rs`（新增 BridgeProvider）+ `src/worker_rpc.rs`（llm_* 消息处理）+ Manager 侧 ProviderBridge

```
worker agent 循环需要补全
  → BridgeProvider.stream() 把请求序列化为 llm_request 写 stdout
  → Manager 收到 → 查 model_whitelist → 用 Mac 本地 provider（真 key）发起
  → llm_chunk × N 流式回写 worker stdin（复用 follow_up 通道的"运行中注入"机制）
  → llm_done（含 usage，归集进 SessionIndex）
异常路径：llm_error（透传 provider 错误，worker 侧复用 ION_LLM_MAX_RETRIES 重试）
打断路径：llm_cancel（用户 abort 时关闭上游 SSE，防漏流烧 token）
```

### 2.4 会话回流

worker 的 `session_jsonl::append_entry` 在 bridge 模式下改为把条目序列化为 `session_entry` 消息写 stdout，由 Manager 落 Mac 本地 JSONL（路径 = Mac 的 `~/.ion/agent/sessions/<cwd_hash>/<sid>.jsonl`，SessionIndex 正常更新）。远端不落盘 → 打穿/销毁不丢历史。

---

## 3. 协议规格（消息字段表）

> 全部消息复用现有 JSONL 信封：`{"id","method/type","params"}`；worker→Manager 请求与 Manager→worker 回包共用 stdio 管道。新增消息类型如下。

### 3.1 能力握手（扩展现有 init）

**Worker → Manager：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `method` | string | 是 | 固定 `"init"` |
| `params.capabilities.provider_bridge` | bool | 是 | true = 请求桥接模式 |
| `params.capabilities.host_call_verbs` | [string] | 是 | worker 支持的动词版本，如 `["v1"]` |
| `params.session_id` | string | 是 | 远端生成的 sid（Manager 据此路由回流条目） |

**Manager → Worker（init 响应）：** `{"granted": ["llm.complete","fs.read","http.fetch"], "session_root": "sess_xxx"}` —— 未授权动词被调用时直接返回 `verb_denied`。

### 3.2 llm_request（Worker → Manager）

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `id` | string | 是 | **request_id**（多路复用关联，必备——协议缺口 #1 修复） |
| `params.model` | string | 是 | 形如 `zai/glm-5.2`；Manager 白名单校验（D4：**无 base_url/api_key 字段，schema 层就不存在**） |
| `params.messages` | [ContentBlock] | 是 | 复用 ion-provider 的 serde 类型，无损往返 |
| `params.max_tokens` / `temperature` 等 | — | 否 | 透传常规补全参数 |

### 3.3 llm_chunk / llm_done / llm_error / llm_cancel（Manager ↔ Worker）

| 消息 | 方向 | 关键字段 | 说明 |
|------|------|---------|------|
| `llm_chunk` | M→W | `id`, `delta`(ContentBlock 流式增量) | 文本/tool_call 增量 |
| `llm_done` | M→W | `id`, `usage{input,output}` | usage 必回传（记账+限额，缺口 #5 修复） |
| `llm_error` | M→W | `id`, `error{kind,message,retryable}` | 原样透传 provider 错误；429/超时标 retryable，worker 侧复用现有重试 |
| `llm_cancel` | W→M | `id` | 用户打断；Manager 关闭上游 SSE |

### 3.4 host_call（Worker → Manager，通道化宿主访问）

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `id` | string | 是 | 请求关联 |
| `params.verb` | string | 是 | 封闭枚举：`fs.read` / `fs.write` / `http.fetch`（新增动词 = 改内核白名单表） |
| `params.args` | object | 是 | 按 verb 定义的参数（见下） |

**verb 参数与默认策略：**

| verb | args | 默认策略 | 限额 |
|------|------|---------|------|
| `fs.read` | `path`, `offset?`, `limit?` | grants.fs_read glob 命中→放行；未命中→PermissionEngine ask（审批 UI） | 单次 ≤ 2MB |
| `fs.write` | `path`, `content` | 同上（fs_write 默认全拒） | 单次 ≤ 2MB |
| `http.fetch` | `url`, `method`, `body?` | 域名 ∈ grants.http_fetch | 速率 + 响应 ≤ 10MB |

**响应**：`{"id","success":true,"data":...}` 或 `{"success":false,"error":"verb_denied\|path_denied\|approval_rejected"}`。每次调用（无论成败）审计为会话 JSONL 的 custom 条目（`customType: "host_call"`，含 verb/args 摘要/决策来源）。

### 3.5 session_entry（Worker → Manager，会话回流）

| 字段 | 类型 | 说明 |
|------|------|------|
| `params.session_id` | string | 远端会话归属 |
| `params.entry` | object | 完整 JSONL 条目（serde 类型复用，无损） |

---

## 4. 关键设计决策点（运行时行为）

| 场景 | 处理 |
|------|------|
| 远端仓库自带 `.ion/hooks.json` / `.ion/rules` | **默认不加载**（D6）：hooks 只认资产包；rules 属仓库内容可加载（只影响提示词，不执行命令）——hooks 与 rules 的区别正在于是否执行 |
| 资产包内容含 Mac 绝对路径 | 下发前 lint 拒绝（便携性纪律，D7）；可插值 `${ION_ASSETS_DIR}` |
| stdio MCP server（npx 类） | 资产包默认只带 http 类 MCP（可移植）；带 stdio 需在资产包声明依赖（远端预装 node） |
| SSH 断线 | = worker 死亡：现有 Dead 保留/父通知逻辑直接复用；资产包目录随清理钩子删除 |
| 审批发生时 Mac 用户不在 | 走现有审批语义（等待/超时），不可信任务建议 grants 全拒 + 显式审批 |
| LLM 桥接依赖 Manager 存活 | Manager 本就是 worker 的父进程，天然同生命周期；无额外可用性损失 |

---

## 5. 安全模型

> 经 4 agent 红蓝两轮对抗评审收敛（2026-09-10），关键结论：

1. **威胁模型**：恶意代码 = worker 执行的不可信任务产物（prompt injection / 恶意仓库 / 供应链），非网络黑客。
2. **客户端模式的隔离完整性**：hooks/LSP/后台进程/spawn_worker 四个"绕层点"在整体搬家中自然消失（worker 全在远端）；worker 无 key（D3）、无 Mac 路径可达（D7）、跨机仅封闭动词（D8/D9）。
3. **剩余攻击面与封顶**：被攻陷的 worker 仍可经 `llm.complete` 烧 token → `llm.max_tokens` 预算封顶；经 `http.fetch` 外传 → 域名白名单；经 `fs.read` 读敏感文件 → grants glob + 审批。所有访问留审计。
4. **与前序安全评审的关系**：本设计同时缓解 hooks 供应链 P0（远端不加载项目级 hooks）、key 扩散（零 key）、Clash 反向通道（http.fetch 收编出网后，Mac 7890 可对局域网关闭）。本地默认态的 hooks 信任门与 config 写保护两个 P0 **仍需独立修复**（见后续工作）。
5. **服务端模式 key 治理**：win38 持 key 属既成事实，短期换专用低额度 key；桥接落地后服务端模式也可切网关方案。

---

## 6. CLI 测试指南

> 自动化脚本：`tests/remote_worker_ci.sh`（实现时落地）。

### Group A：远程拉起与生命周期

```bash
# A1 拉起远程 worker（spawn_worker 已有工具，新增 host 参数）
ion rpc --method spawn_worker --params '{"agent":"build","host":"win38","message":"uname -a"}'
```
**预期**：响应含 worker id 与 `host:"win38"`；`list_workers` 可见；事件流正常（agent_start→text_delta→agent_end）。

```bash
# A2 SSH 断线 = worker 死亡
ssh win38 "pkill -x ion"   # 杀远端 worker
ion rpc --method list_workers
```
**预期**：worker 转 Dead，不复活、不悬挂。

### Group B：LLM 桥接（零 key 验证）

```bash
# B1 远端会话真实对话（win38 侧无 ~/.ion/config.json 的 key 也可用）
ion rpc --method spawn_worker --params '{"host":"win38","message":"1+1=? 只回答数字"}'
# B2 验证 key 未过端：远端查配置
ssh win38 "wsl -d ion -- cat /root/.ion/config.json"   # 无 api_key 字段
# B3 验证 usage 回传与记账
ion rpc --method list_sessions                          # 远端会话 token 统计在 Mac 侧可见
```
**预期**：B1 正常作答；B2 无 key；B3 SessionIndex 有 token 记录。

### Group C：会话回流与资产包

```bash
# C1 会话落在 Mac
ion rpc --method get_session_messages --params '{"session":"<远端sid>"}'   # Mac host 直读可见
# C2 资产包：远端只见到下发的 skills
ssh win38 "wsl -d ion -- ls /tmp/ion-assets-<sid>/skills"
# C3 远端项目级 hooks 不加载：构造带恶意 .ion/hooks.json 的仓库在远端分析，观察 hooks 日志
```
**预期**：C1 完整历史；C2 仅白名单子集；C3 hooks 零加载记录。

### Group D：动词表（通道化访问）

```bash
# D1 fs.read 未授权路径 → 审批流
ion rpc --method spawn_worker --params '{"host":"win38","message":"读取 /Users/xuyingzhou/Project/study-rust/ion/Cargo.toml 的前 5 行"}'
# D2 grants 白名单路径 → 直接放行（免审批）
# D3 http.fetch 非白名单域名 → verb_denied
# D4 审计：会话 JSONL 含 host_call custom 条目
grep host_call ~/.ion/agent/sessions/**/<sid>.jsonl | head
```
**预期**：D1 触发审批 UI；D2 成功；D3 拒绝且报 `verb_denied`；D4 每次调用有审计。

---

## 7. 测试方案（harness + e2e）

| 层 | 方案 | 说明 |
|----|------|------|
| Harness（必须） | **FauxProvider 放 Manager 侧**：远端 worker 的 BridgeProvider 发 llm_request，Manager 用 Faux 队列应答 | 桥接全链路零 API 成本；多路 request_id 关联、cancel、error 重试均可注入测试 |
| Harness（动词） | 假 fs/http 动词 + grants 配置矩阵（命中/未命中/审批/限额） | ACL 语义全覆盖 |
| CI 脚本 | `tests/remote_worker_ci.sh`（Group A-D 全量，起 host + 断言） | 命令行可验证原则 |
| 真实 e2e（最后补） | `#[ignore]` + `ION_E2E=1`，真实 zai/glm-5.2 走桥接 | 验证真实 SSE 流经管道无损 |

---

## 8. 实现路线

| 里程碑 | 内容 | 量级 |
|--------|------|------|
| **M1** | SSH spawn 适配器 + remote_workers 配置 + 远程 worker 生命周期（含真 key 的过渡形态：远端 config 暂用专用 key） | ~150 行 |
| **M2** | LLM 桥接：BridgeProvider + llm_* 五消息 + model 白名单 + usage 归集 | ~400 行 |
| **M3** | 会话回流 + 资产包下发 + 便携性 lint | ~250 行 |
| **M4** | 动词表流水线（VerbGate：策略/审批/审计/限额）+ fs/http 动词 | ~300 行 |
| 后续 | 多机配置化推广（NAS 等任意 Linux 机）、执行端主动外连模式（穿 NAT，对标 broker 方案） | — |

依赖关系：M1 独立可用；M2 依赖 M1；M3/M4 依赖 M2（回流与审计复用管道）。

---

## 9. 开放问题

| # | 问题 | 当前倾向 |
|---|------|---------|
| 1 | 远端 worker 的 LSP/Monitor 是否默认禁用 | 倾向默认禁（不可信任务），trusted host 配置可开 |
| 2 | `fs.read` 审批的粒度（每次 vs 每路径 stored decision） | 复用现有 stored-decision 语义，但 deny 永不被覆盖（对接安全评审 P0#2） |
| 3 | 资产包版本化（skill 更新后旧会话回放） | 先不做，会话 JSONL 已含注入记录可追溯 |
| 4 | 动词表是否未来外化为 Mac 侧 MCP server | 保持内核管道（少一个常驻服务）；协议形状兼容，方向留着 |

---

## 附：市面对标索引

- GitLab Runner（job token / 日志回中心）/ GitHub Actions self-hosted runner —— 客户端模式
- Bazel REAPI（hermetic 输入 / 结果回 CAS）—— 资产包 + 无状态执行端
- K8s ConfigMap + kube-apiserver —— 资产下发 + 一切经 API server
- LiteLLM / OpenRouter（AI Gateway）—— 中心化 key + 虚拟 key 思路（桥接的 provider 侧替身）
- MCP consent / 对象能力模型（no ambient authority）—— 封闭动词表
- 自托管 n8n / JupyterHub —— 服务端模式
