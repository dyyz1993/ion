# pi UI 形态调研（upstream/main@f9bcd351d, v0.85.1）

> **状态：已完成（调研）** — 基于 pi-mono-upstream-main 源码只读调研（2026-09-17），回答四个问题：pi 有没有官方 Web UI、UI 接线架构、多端 attach 机制、对 ION 的启示素材。不做 ION 侧结论。

> **术语约定**：pi 的扩展系统叫 **Extension**，本调研沿用该术语；ION 侧同名词汇见根目录 [AGENTS.md](../../AGENTS.md) 术语规范。

---

## 概览

pi 当前（v0.85.1）**没有官方 Web UI**。UI 形态以 TUI 为绝对主形态（`packages/tui` 终端 UI 库 + `coding-agent/src/modes/interactive/` 6600 行 interactive-mode，**进程内直连 AgentSession**）。但仓库里正在孵化一套**实验性 client/server 架构**（`packages/{protocol,server,client,chord}` + `coding-agent/src/experimental/`，约 15k 行），其设计目标明确指向"worker / TUI / remote WebUI 三环境"，并提供 Unix socket + **Radius WebSocket 中继（外网可达）**两条传输。这是"给 WebUI 铺路"的部分——但今天唯一的 presentation 消费者仍是终端 TUI。

| UI 形态 | 入口 | 状态 | 接线方式 |
|---------|------|------|---------|
| 交互式 TUI（主形态） | `pi`（default mode） | ✅ 生产 | **进程内**直连 `AgentSession`，不经任何协议 |
| print / json / rpc 模式 | `pi -p` / `--mode json` / `--mode rpc` | ✅ 生产 | stdin/stdout JSONL（ION 已对齐的 `pi --mode rpc`） |
| 实验性 client TUI | `pi client`（dev-only，需开实验开关） | 🔧 实验 | Unix socket / Radius WS → pi-server → session-worker（Chord 服务 + 复制状态） |
| 实验性 headless client | `pi client "prompt"` | 🔧 实验 | 同上，非 TTY 时直接流式打印 text_delta |
| mini 三进程示例 | `node .../mini/main.ts` | 🔧 参考实现 | 自带简化 RPC：server + 每 session 一 worker + 多 TUI presentation |
| Web UI | **不存在** | ❌ | CHANGELOG 明确记录"Removed web UI workspace references"（v0.85.0 前后清理） |
| Slack/chat 自动化 | 外部仓库 `earendil-works/pi-chat` | 外部 | 根 README 明确导流到独立仓库 |

---

## 1. pi 有没有官方 Web UI？

**结论：没有，且刚被主动清理过。**

- 全仓 grep `webui|web ui`：仅命中 `packages/coding-agent/CHANGELOG.md`（"Removed web UI workspace references from the CLI package and dropped the package-level development watch script"，见 CHANGELOG L1432，属于 v0.85 清理批次）和 `packages/chord/README.md`（Chord 动机描述里的 "a remote WebUI"）。
- 根 README 的 All Packages 表只列 chord/telemetry/ai/agent/coding-agent/tui 六个正式包；`server`、`client`、`protocol` 三个包**不在官方包列表里**，README 自述 "Experimental local server for the new durable Session and Agent Harness interfaces"，且 protocol README 声明 "experimental and has no compatibility guarantees"。
- coding-agent 的对外形态文档（`docs/usage.md` §Modes）只列五种：default 交互 TUI、`-p` print、`--mode json`、`--mode rpc`、`--export` HTML 导出。没有任何 web 模式。
- 会话分享走静态 HTML 导出（`--export`，浏览器只做离线查看）+ 外部 `pi-share-hf`（HuggingFace 发布），不是在线 UI。

**但 server/client/protocol + chord 这套确实是为"多 presentation（含 WebUI）"铺路**，证据链：

1. `packages/chord/README.md` 开篇动机原话："A single application feature may need to run in several environments: for example, an agent worker, a terminal UI, and a **remote WebUI**. Chord provides the generic machinery to write such extensions…"
2. `packages/client/README.md`：transport-neutral，"Connect using **WebSocket**, Unix socket, or another ordered byte transport"——WS 是一等公民传输。
3. `packages/coding-agent/src/experimental/radius-relay.ts`：server 主动向 `https://radius.pi.dev`（`PI_RADIUS_GATEWAY` 可覆盖）**拨出** WebSocket 中继连接（子协议 `pi-session-relay.host.v1` / `client.v1`），远端客户端经 Radius 网关接入本地 server——这就是"手机/浏览器不开端口直连家里 pi"的通道。CLI 侧 `--connect radius://<serverId>` + `--auth-token` 即可走这条线（`command-options.ts` 的 `RadiusTransportAddress`）。
4. `packages/coding-agent/src/experimental/mini/` 是一个刻意的参考实现："Starting `mini` twice attaches **two presentations** to the same session; both see the same transcript, live."（mini/README.md）

---

## 2. UI 接线架构

### 2.1 两套并存的接线（关键结论）

| | 生产 TUI（interactive-mode） | 实验 client TUI（experimental/client-tui.ts） |
|---|---|---|
| Agent 消费 | 进程内 `AgentSession`（`modes/interactive/interactive-mode.ts` 6620 行，`delegating business logic to AgentSession`） | 远端 Chord 服务：`Transcript`（复制状态订阅）+ `AgentController`（RPC 门面） |
| 组件复用 | 原生 | **复用同一套组件**（`createChatViewport` / `CustomEditor` / 主题 / `AssistantMessageComponent` / `ToolExecutionComponent` 全部从 `modes/interactive/` import） |
| 状态来源 | 直接读 AgentSession | `LaneSnapshot` 快照 + 事件流，渲染是快照的纯函数 |
| 进程拓扑 | 单进程 | client ↔ coordinator ↔ server ↔ session-worker 四段（见 2.2） |

### 2.2 实验架构的进程拓扑与传输

```
presentation (pi client / 未来 WebUI)
    │  pi-protocol: CBOR 帧 over Unix socket 或 Radius WebSocket 中继
    ▼
coordinator（纯传输 shim，独立进程，不解析任何业务 payload）
    │  公共 socket = 稳定端点，字节 pipe 到当前 server；控制 socket 做 server 换代仲裁
    ▼
pi-server 进程（可替换、可自动冷启动：~/.pi/server/<serverId>.sock）
    │  路由信封 { serverId, sessionId, attachmentId }，不解码业务 payload
    ├─ server 级服务：SessionDirectory(复制状态) / SessionManagement / PresentationPlugins
    └─ 转发到 session-worker（每 session 一个子进程，经 coordinator 的 peer 通道）
         └─ 真正的 AgentHarness + JsonlSessionRepo + Chord FacetHost
              提供 AgentController / Transcript / Models / SlashCommands / 插件 session facet
```

要点：

- **coordinator 是透明的**（coordinator.ts 注释原文 "intentionally a transport shim. It depends only on Node built-ins and never interprets Pi, session, worker, or lifecycle payloads"）。server 可以被替换/重启而不换公共端点；server 断开时 coordinator 通知所有 peer（`server_disconnected`），新 server 注册时旧者收到 `server_replaced` 自动退位。空置 30s 自动自杀。
- **server 生命周期是 demand 驱动的**（server.ts `ServerLifetime`）：连接数 + worker 活动数双计数，归零后 1s 宽限自动退役；启动后 10s 宽限期内无连接即退。客户端 `activateServer()` 发现连不上就 `spawnInternalProcess("server", …)` 自动拉起——**server/worker 生命周期刻意留在公共协议之外**（protocol README 原文），每个应用自管。
- **worker 退役也是 demand 仲裁**（session-worker.ts `WorkerLifecycle`）：`{serverConnectionId, attachmentId}` 二元组的 demand 集合 + 进行中操作（run/compaction/navigation），server 换代后孤儿 demand 有宽限期，全部归零才 retire。陈旧 server 代的请求直接拒绝（`stale server generation`）。

### 2.3 Chord 在 UI 侧的用法（facet + 复制状态）

Chord（`packages/chord`，自称"不是 Pi 包，可被无关应用使用"）提供四块机器，UI 侧各有一处具体消费：

**(a) Facet = 插件的多环境切片。** 一个插件包可声明多个 facet 入口（`package.json` 的 `chord.facets`，如 `worker` / `session` / `presentation`），esbuild 打成独立 content-addressed `.cjs`（`@earendil-works/chord/bundler`），各环境各取所需加载：
- `session` facet → worker 进程内加载（`plugins/bundled.ts` `createSessionPluginFacetLoader`，entry=`session`）
- `presentation` facet → **server 打包成 artifact，经 `PresentationPlugins` 服务下发给客户端**，客户端用 `createFacetBundleArtifactLoader`（SHA-256 校验 + `node:vm` 编译，不经模块缓存）在 UI 进程内加载（client-tui.ts `#start`）。**即：一个插件的 UI 代码不是预装在客户端里的，而是由 server 按 session 动态下发、可热 reload（`FacetHost.reload` 无缝换代）。**
- presentation facet 在 UI 内通过 **`PresentationUI` 本地服务**（`services/presentation-ui.ts`，`defineService(..., { local: true })`）回调宿主 UI：`select()` 弹选择器、`showStatus()` 写状态栏——client-tui 的 `presentationBridgeFacet` 提供实现。插件 UI 代码拿到的 API 与宿主无差别。

**(b) Replicated state + delta = 流式 UI 的数据底座。** 生产者持有 tracked proxy，改完 `publish()` 一次 flush 一个操作批；字符串追加被识别为 append 操作（不重发全文）、数组追加识别为 append、滚动窗口识别为 front-truncate。每个订阅方持独立 path-codec 字典。UI 侧典型消费（transcript-provider.ts）：worker 内 `lane.watch()` 两阶段（快照+缓冲事件，无缝无重）→ 每个事件 `reduceLaneSnapshot` 折叠进快照，返回 `rebase` 时自动 resnapshot。客户端 `transcript.state.subscribe((value, _ctx, delivery) => ...)` 拿到的永远是"全量快照值 + 本次 delta"。

**(c) 服务 token = 类型化远程门面。** `defineService<T>("pi.transcript")` 等；会话域五个核心服务（`services/*.ts`）：

| 服务 | kind | 成员 |
|------|------|------|
| `pi.transcript` | 复制状态 | snapshot（LaneTranscriptSnapshot）+ event |
| `pi.agent-controller` | RPC | prompt / requestAbort / steer / followUp / nextRun / cancelQueued / resume / compact / navigate |
| `pi.models` | RPC+状态 | 目录/账号/登录 |
| `pi.session-directory` | 复制状态 | `{revision, sessions[]}`（目录投影为 presentation-safe 状态，server README："projects the private catalog into replicated presentation-safe state"） |
| `pi.session-management` | RPC | create / remove / attach / detach（**返回值不含路由 ID**，attachmentId 走带外 attachment 消息下发） |
| `pi.presentation-ui` | **local-only**（UI 进程内） | select / showStatus（给下发的 presentation facet 回调宿主） |

**(d) 远程边界 = 严格 JSON。** Chord 只定服务线语法（`$chord.service` 控制词汇、快照/更新编解码、错误码），不订帧/路由/外层信封；`pi-protocol` 只验信封与 strict-JSON（16MiB 上限、拒绝非 JSON 值），CBOR 编帧。业务 payload 对路由层全程不透明——"Neither an open JavaScript Session nor a Harness crosses the process boundary"（server README）。

### 2.4 渲染效率模型（mini README 的量化表述）

- 快照按 entry-id 键控追加："a streamed token costs **one markdown rebuild** rather than a repaint"——流式 token 只重建当前条目的 markdown，不整屏重绘。
- 只有 entry-id 前缀分叉（compaction、导航）才整树 repaint（client-tui-chat.ts `#syncTranscript` 的 diverged 检测同款逻辑）。
- pi-tui 本身是差分渲染（只更新变化的行，CSI 2026 同步输出），两层差分叠加。

---

## 3. 多端 attach（一个 Session 多个 presentation）

**机制**（server README + protocol README + 代码）：

1. 客户端连接后经 `SessionManagement.attach(sessionId)` 请求挂接；server 的 `RoutedSessionHandle.attachClient()` 返回 presentation-scoped 的 Session 能力端点。**同一连接重复 attach 幂等；每个成功 attach 发一个 server 生成的 `attachmentId`（仅作为路由控制数据带外下发）。**
2. 之后该 presentation 的所有 session 请求都携带 `{serverId, sessionId, attachmentId}` 三元组；server 拒绝 stale/错配路由（`SessionRouter` 语义），**断线只释放自己的 attachment**（已受理调用 settle 之后），其他 presentation 不受影响。
3. **订阅按 attachment 隔离**："service subscription updates remain scoped to the requesting attachment"。每个 presentation 在 worker 里有自己独立的 lane 订阅（mini README："Each presentation gets **its own** `lane.watch()` in the worker"），所以两个 TUI 同时开着都实时看到同一条 transcript。
4. worker 的 retirement 按 attachment demand 仲裁（2.2 末）——**host 决定"零 presentation 需求 + worker 空闲"时才收走 worker**。
5. 实证参考是 mini：开两个 `mini` 即两个 presentation 同看一个 session，事件广播（mini 自认的捷径：N 个 presentation 事件走 N²，各自丢弃非自己的拷贝——正式 pi-server 已改为按 attachment 定向）。

**移动端/浏览器路径**：协议层已就绪——`ByteTransportFactory` 显式支持 WebSocket（client README），Radius 中继让本地 server 反向拨出 WS、远端客户端凭 OAuth token 接入（radius-relay.ts + `RadiusClientReconnect`：radius 断线重连后自动重新 attach 会话）。缺的只是**一个真的浏览器 presentation 实现**——仓库里没有；UI 组件库（pi-tui）是终端专用的，浏览器端需要另写渲染层（消费的还是同一组服务 token 与快照）。

---

## 4. 对 ION 的启示素材（只摆事实，不做结论）

以下是 pi UI 形态中 ION 两个 UI（ion-webui 控制台 / ion-web 沙盒产物）现状**没有对应物**的"消费者可见体验"，全部有源码出处：

1. **流式 = 增量 delta 而非全量消息重发**。pi 的 Transcript 复制状态把流式 token 编码为字符串 append 操作、每个订阅方独立 codec（chord README §Delta tracking；mini README"一个 token 一次 markdown 重建"）。ION 现状：webui 靠 subscribe 事件流逐帧转发 text_delta（自定义事件帧），刷新恢复靠 get_messages 拉全量快照；pi 的模型里"快照 + 增量流"是同一个订阅的两阶段，首连与 rebase 是同一个函数（mini README），不存在两套通道。
2. **一个插件同时出 worker 侧与 UI 侧代码**。Chord facet 把一个插件切成 `session`（worker 加载）与 `presentation`（server 下发 artifact、UI 进程内加载、支持热 reload 且换代期间旧 provider 保持路由）两片，UI 片还能通过 `PresentationUI` 本地服务弹宿主选择器/写状态栏（client-tui.ts `presentationBridgeFacet`）。ION 现状：WASM 扩展只有 worker 侧形态，UI 呈现靠约定 customType 由前端硬编码渲染。
3. **presentation 是可插拔、可多挂的**。同一 session 的 N 个 attachment 各持独立订阅、互不干扰、断线只拆自己（server README；protocol README attachment 语义）。ION 现状：subscribe 是 per-session epoch 绑定 + stale_route 作废重订（SUBSCRIBE_PROTOCOL.md），多终端同步靠 EventBus 广播同一事件流，但没有"每个 presentation 独立 attachment 生命周期 + attachment 定向订阅"这一层。
4. **server/worker 生命周期完全协议外置 + demand 驱动**。coordinator 只做透明路由，server 可被任意替换（`server_replaced` 平滑让位），连接数/活动数归零自动退役、下次连接自动冷启动（server.ts `ServerLifetime`、`activateServer`）。ION 的 host 是显式常驻（`ion serve` 手动 shutdown），worker 重建是 ensure_live 三段决策（修复批 6），没有"server 进程本身可无感换代、端点永不换"的设计。
5. **远端接入走"server 反拨出站 WS + 网关中继"**，客户端不需要能访问本机端口（radius-relay.ts；`--connect radius://serverId`）。ION 现状：webui 需要能连到 host socket（Python 网关本地代理），Remote Worker 反向是 ION 的 worker 搬到远端，方向相反。
6. **UI 与服务解耦到"同一组组件喂两种数据源"**。实验 client TUI 直接复用生产 interactive-mode 的 viewport/编辑器/消息组件（client-tui.ts imports），只是把数据源从进程内 AgentSession 换成快照订阅——pi 用这个约束验证了"presentation 只需要 LaneSnapshot + AgentController 这么窄的接口"（mini README 的立项目的就是"find out what an RPC-shaped presentation actually needs"）。ION 现状：CLI 侧与 webui 侧是两套独立实现（ion-console / ion-webui）。
7. **session worker 每会话一进程、存储与 harness 都在 worker 内**，presentation 永远拿不到 JS 对象只拿快照（server README"Neither an open JavaScript Session nor a Harness crosses the process boundary"）。ION 是 worker-per-session（同构），但 host 直读走磁盘（host_read），pi 的等价物是 SessionDirectory 复制状态投影。

### 关键文件索引（pi-mono-upstream-main）

| 文件 | 内容 |
|------|------|
| `packages/chord/README.md` / `PLANNING.md` | facet/服务/复制状态/delta 的完整规范与动机（含 WebUI 原话） |
| `packages/protocol/README.md` | 路由信封 `{serverId, sessionId, attachmentId}`、CBOR 帧、协议版本 8 |
| `packages/server/README.md` | multi-presentation attach、demand 生命周期、opaque 路由 |
| `packages/client/README.md` | transport-neutral 客户端、WS/Unix、发现、断线语义 |
| `coding-agent/src/experimental/coordinator.ts` | 透明路由 shim（server 换代仲裁、空置自杀） |
| `coding-agent/src/experimental/server.ts` | server 进程编排：lockfile 选主、自动冷启动、插件包管理、Radius host |
| `coding-agent/src/experimental/session-worker.ts` | worker demand 仲裁 + 经 coordinator peer 通道的私有服务协议 |
| `coding-agent/src/experimental/services/*.ts` | 六个服务 token 定义 + worker 侧 provider（transcript-provider 是复制状态样板） |
| `coding-agent/src/experimental/client-tui.ts` / `client-tui-chat.ts` | 服务驱动的 TUI presentation（facet 装载、PresentationUI 桥、快照渲染） |
| `coding-agent/src/experimental/client-runtime.ts` | 客户端运行时：unix/radius 双路由发现与服务命名空间 |
| `coding-agent/src/experimental/radius-relay.ts` / `radius-auth.ts` | WS 中继 host/client 两侧 + OAuth 凭据（默认网关 `https://radius.pi.dev`） |
| `coding-agent/src/experimental/mini/`（README 尤佳） | 三进程参考实现：多 presentation、快照复制、渲染效率模型 |
| `coding-agent/CHANGELOG.md` L1432 | "Removed web UI workspace references"——web UI 清理的记录 |

---

## 附：事实核查清单（防以讹传讹）

- "pi 官方支持 Web UI" ❌ —— 无此包；server/client/protocol 全部自述 experimental，dev-only CLI（`runExperimentalCommand` 需 `areExperimentalFeaturesEnabled()`，且注释 "Published entrypoints must not import this module"）。
- "pi-server 是稳定 RPC 面" ❌ —— protocol README："experimental and has no compatibility guarantees"；稳定的是 pi --mode rpc（stdin/stdout，生产级，ION 已对齐）。
- "Radius 是模型网关" ✅ 且不止 —— `docs/providers.md`：Radius 是动态 `pi-messages` 网关（模型目录）；同一网关同时承担 session-relay 中继（radius-relay.ts 复用 `DEFAULT_RADIUS_GATEWAY` 与 radius OAuth）。
- "presentation facet 是浏览器代码" ❌（当前）—— 下发/装载机制就绪，但现有 presentation 只有终端 TUI；chord PLANNING.md 把"browser-compatible core 是否为即时测试要求"列为未决问题（§16.9）。
