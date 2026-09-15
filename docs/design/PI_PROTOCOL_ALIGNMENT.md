# pi protocol 包对齐文档 — 信封/帧/CBOR 协议深读与 ION 映射

> **状态：已完成（调研）** — pi 上游 `pi-mono-upstream-main`（upstream/main@f9bcd351d）`packages/protocol` 全量源码 + `chord`/`server`/`client` 结构面深读完成；ION↔pi 映射、adopt/avoid 建议与简化安全性评估成文。P0 落地项未实施。
>
> **术语约定**：pi 的扩展系统叫 Extension（不叫 Plugin）。本文统一使用 extension 术语。

---

## 一、背景

pi 新版将"远程会话协议"抽成了独立包 `@earendil-works/pi-protocol`（v0.85.1，协议版本 **8**），与 payload 语法层 `@earendil-works/chord`、路由传输层 `@earendil-works/pi-server`/`@earendil-works/pi-client` 三层分工。ION 侧已在 master@274d909 落地对标的订阅协议（epoch 栅栏 + 快照先行 + hello 握手 + 审批同源，见 [SUBSCRIBE_PROTOCOL.md](./SUBSCRIBE_PROTOCOL.md)），实现走的是现有 JSON 行协议而非二进制帧。

本文回答三个问题：
1. pi protocol 包**每个类型/函数到底做了什么**（含校验规则、帧格式、错误模型的 file:line 证据）；
2. pi 的每个协议概念在 ION 的对应物是什么、差距是否需要补；
3. 哪些值得 adopt（按成本/收益排级）、哪些明确 avoid，以及 ION 已做的简化是否安全。

**pi 源码位置**：`/Users/xuyingzhou/Project/temporary/pi-mono-upstream-main/`（upstream/main@f9bcd351d，只读 study worktree）
- protocol 包：`packages/protocol/src/{protocol,codec,framing}.ts` + `src/cbor/`
- payload 语法：`packages/chord/src/services/wire.ts` 等
- 路由传输：`packages/server/src/`、`packages/client/src/`
- 应用消费：`packages/coding-agent/src/experimental/`（实验态入口）

---

## 二、pi 当前状态

### 2.1 三层分工的精确边界

| 层 | 包 | 拥有什么 | 明确拒绝拥有什么 |
|----|----|---------|----------------|
| **信封层** | `pi-protocol` | 协议版本常量、信封 schema（request/response/cancel/service_update/attachment/hello）、路由 target 形状（serverId/sessionId/attachmentId）、错误信封形状（`{code,message}`，code 为**不透明非空字符串**）、CBOR 严格子集编解码、4 字节大端长度帧、16MiB 帧上限、未知字段拒绝、opaque payload 的 strict-JSON 校验 | 不解释任何 payload 语义（`call`/`update`/`result` 全部 opaque）；不做 peer 认证（README:39 明说未实现）；不管 server/worker 生命周期（README:19："experimental local coordinator is only an opaque message router; each replaceable server process owns the private lifecycle protocol"） |
| **payload 语法层** | `chord` | `$chord.service` 控制词汇（catalogue/subscribe/unsubscribe）、ServiceCall 形状 `{serviceId, instance?, member, args}`、服务目录、订阅快照/增量（delta path codec）、服务错误码枚举（8 个）、strict-JSON 值校验 `isJsonValue` | 不管信封、路由、帧格式、传输（README:69-87："Chord does not prescribe that outer protocol"）；不管应用数据含义 |
| **路由与传输层** | `pi-server` / `pi-client` | 连接状态机（awaitingHello→handshaking→ready）、hello 时序仲裁（5s 握手超时）、按 target 路由（server 级 vs Session 级）、attachmentId 能力签发与带外发布、订阅快照-增量顺序保证、取消传播（AbortController）、错误码到信封的映射与内部错误脱敏、Unix socket 传输 | 不定义 service 语义（透传给 chord adapter）；不拥有 Session/AgentHarness（process-local，README:17）；不做认证 |

**应用侧消费方式**（证实实验态）：`coding-agent/package.json` 中 `pi-protocol`/`pi-server`/`pi-client` 均在 **devDependencies**（非 dependencies）；真正的消费代码全部在 `coding-agent/src/experimental/`（`server.ts`、`client-runtime.ts`、`session-worker-manager.ts`、`radius-relay.ts`、`services/`）；`coding-agent/src/client/index.ts` 只有一行 `export * from "@earendil-works/pi-client"`。protocol 包自述 "experimental and has no compatibility guarantees"（README:41）。

### 2.2 protocol 包逐文件事实清单

#### `src/protocol.ts`（111 行，纯 schema + 类型）

| 类型/常量 | 职责与校验规则 | 证据 |
|-----------|--------------|------|
| `PROTOCOL_VERSION = 8` | 协议版本常量；客户端 hello 里 version 是**任意整数 ≥0**（允许客户端声明更新版本协商），服务端 hello 里是**字面量 8** | protocol.ts:5, 29-32, 65-69 |
| `IdSchema` | 非空字符串（request id / sessionId / attachmentId / subscriptionId 通用） | protocol.ts:7 |
| `OpaqueJsonValueSchema` | `Type.Unsafe<JsonValue>(Unknown)`——类型上标成 JsonValue，**实际校验交给 codec 层的 `isJsonValue`**（schema 层不校验） | protocol.ts:8; codec.ts:21 |
| `StrictObject` | 所有信封对象统一 `additionalProperties: false`——**未知字段一律拒绝** | protocol.ts:9-10 |
| `ServerIdSchema` / `isServerId` | **规范小写 UUIDv4** 正则 `^[0-9a-f]{8}-[0-9a-f]{4}-4...-[89ab]...-...$`（version 位=4、variant 位=8/9/a/b、全小写），大写或非规范形式直接拒 | protocol.ts:12-19 |
| `ProtocolErrorSchema` | `{code: 非空字符串, message: string}`，strict；`ProtocolErrorCode = string`——**错误码是不透明的**，协议层不枚举（测试明确接受 `wrong_server`/`cancelled`/`service_not_found`/`application_error` 等任意非空串） | protocol.ts:21-26; protocol.test.ts:192-203 |
| `ClientHello` | `{type:"hello", version: integer≥0}` | protocol.ts:29-33 |
| `ServerTarget` | `{serverId}`——server 级调用（模型列表等全局服务） | protocol.ts:36-38 |
| `SessionTarget` | `{serverId, sessionId, attachmentId}`——**三元路由栅栏**：一个逻辑 server + 一个持久 Session + 一个**活着的** presentation attachment | protocol.ts:40-45 |
| `RequestEnvelope` | `{type:"request", id, target, call: opaque}` | protocol.ts:49-54 |
| `CancelEnvelope` | `{type:"cancel", id, target}`——与 request 同 id 同 target 才命中 | protocol.ts:55-61 |
| `ClientMessage` | `hello \| request \| cancel` 三选一 union | protocol.ts:62-63 |
| `ServerHello` | `{type:"hello", version: 8(字面量), serverId}`——**握手同时宣告逻辑身份** | protocol.ts:65-69 |
| `ServerHelloError` | `{type:"hello_error", error}`——握手失败终帧 | protocol.ts:70-73 |
| `ResponseEnvelope` | 按 `ok` 判别：`ok:true` 时 `result` **可选**（void 响应合法，直接省略字段）；`ok:false` 时 `error` 必填 | protocol.ts:74-87; protocol.test.ts:176-182 |
| `ServiceEventEnvelope` | `{type:"service_update", subscriptionId, update: opaque}`——订阅增量 | protocol.ts:88-92 |
| `AttachmentEnvelope` | `{type:"attachment", attachment: SessionTarget \| null}`——**带外**路由变更（null = 解绑）；"Out-of-band update to this presentation's selected Session route" | protocol.ts:93-97 |
| `ServerMessage` | `hello \| hello_error \| response \| service_update \| attachment` 五选一 | protocol.ts:98-110 |

#### `src/codec.ts`（142 行，校验 + 编解码门面）

| 函数/类 | 职责 | 证据 |
|---------|------|------|
| `ProtocolValidationError` | 统一协议错误类型：信封违规、坏 CBOR、坏帧都抛它 | codec.ts:13-18 |
| `parseClientMessage` / `parseServerMessage` | **双闸校验**：typebox `Check(schema)`（信封形状+未知字段拒绝）**且** `isJsonValue(value)`（递归 strict-JSON：拒绝 byte array、NaN/Infinity、undefined 属性、循环引用、非 plain-object 原型） | codec.ts:20-32; protocol.test.ts:99-122 |
| `boundedErrorMessage` | 错误消息**截断到 500 字符**（"bounded transport messages"）——防止内部错误细节/超长串跨协议边界 | codec.ts:34-37 |
| `encodeClientMessage` / `encodeServerMessage` | 先 parse 校验 → CBOR 编码（maxByteLength=帧上限）→ 加 4 字节长度头；编码侧也强制帧上限（outbound limit） | codec.ts:39-63; protocol.test.ts:224-227 |
| `ValidatedMessageDecoder` | 增量解码 + **毒丸语义**：一旦失败，后续所有 `push`/`end` 永久抛 "decoder has failed"——坏流之后不许继续消费 | codec.ts:65-103; protocol.test.ts:265-273 |
| `ClientMessageDecoder` / `ServerMessageDecoder` | 两端各自的有类型增量解码器（任意分片/合包） | codec.ts:106-137 |
| `isSupportedProtocolVersion` | `Number.isInteger(v) && v === 8`——严格相等，不做向后兼容区间 | codec.ts:139-141 |

#### `src/framing.ts`（152 行，二进制帧）

| 项 | 事实 | 证据 |
|----|------|------|
| 帧格式 | **4 字节无符号大端长度前缀 + 一个定长 CBOR item** | framing.ts:1, 28-39; README:21 |
| 默认上限 | `DEFAULT_MAX_FRAME_LENGTH = 16 MiB`（帧/CBOR payload 同值） | framing.ts:6; options.ts:6 |
| 超限拒绝时机 | **头部一读完立即拒**（不等 payload 到齐）——`frameLength > maxFrameLength` 即 `fail()` | framing.ts:77-79 |
| 零长帧 | 合法，直接产出空 payload（留给上层 CBOR 校验去拒） | framing.ts:80-83 |
| 拷贝语义 | payload 按 64KiB block 组装并**拷贝**，不 alias 输入 chunk（测试有 anti-aliasing 断言） | framing.ts:3, 93-127; framing.test.ts:62-68 |
| 状态机 | `open/ended/failed` 三态；`end()` 时若有半截帧 → FrameError；failed 后一切操作抛错 | framing.ts:41-151 |
| 配置校验 | `maxFrameLength` 必须 0..2^32-1 的安全整数，否则 RangeError | framing.ts:19-25; framing.test.ts:104-109 |

#### `src/cbor/`（445 行，严格 RFC 8949 子集）

| 项 | 事实 | 证据 |
|----|------|------|
| 默认限额 | byte 16MiB / container 1,000,000 元素 / 深度 64（硬顶 512）——"Safe defaults for untrusted protocol payloads" | options.ts:3-8 |
| 编码端拒绝 | 非有限数、非安全整数、-0 作整数、undefined/空洞数组元素、symbol key、循环引用（ancestors 集合）、非法 Unicode 标量（round-trip 校验） | encoder.ts:126-207 |
| 解码端拒绝 | **尾随数据**（一个 item 后必须 EOF）、CBOR tag（major 6）、不定长（indefinite）、break marker、非有限浮点、超安全范围整数、**重复 map key**、非字符串 key | decoder.ts:22, 68, 80, 105-108, 113, 135-139 |
| UTF-8 | fatal 解码器 + ignoreBOM，坏 UTF-8 拒绝 | options.ts:33; decoder.ts:49-53 |

**错误码模型总结**（三层合计）：
- 信封层只有形状没有枚举：`ProtocolErrorCode = string`（非空即可）。
- 传输层（server）实际使用的码：`invalid_request` / `version` / `cancelled` / `internal_error`（server.ts 内联）+ `wrong_server` / `session_not_found` / `session_ambiguous` / `session_not_attached` / `server_draining`（server/src/errors.ts:3-9）+ chord 的 8 个 `RemoteServiceErrorCode`（services/errors.ts:2-11）。
- 内部错误统一脱敏：`toProtocolError` 把非 ServerError/RemoteServiceError/ProtocolValidationError 的一切映射为 `{code:"internal_error", message:"Internal server error"}`（固定文案），原始错误只进 `onError` 观察口——**内部细节不过协议边界**（server.ts:512-521）。

### 2.3 server 侧行为事实（`packages/server/src/`）

- **握手状态机**：`awaitingHello → handshaking → ready`（connection.ts:21）；首条消息非 hello → `invalid_request` 终止（server.ts:224-231）；hello 之后再来 hello → 拒（server.ts:239-245）；版本不符 → `hello_error {code:"version"}` 后**关闭连接**，`hello_error` 作为关闭前最后一条帧发送（server.ts:262-269, 474-487）；握手默认超时 **5s**（server.ts:42）。
- **取消**：cancel 需 `serverId` 匹配且 id 命中活动请求且 **target 三元组完全一致**（`sameTarget`，server.ts:298-304, 547-553）；不匹配**静默忽略**（不报错不断连）；命中则 AbortController.abort。
- **重复请求 id**：同连接同 id 仍活动 → 直接回 `{ok:false, code:"invalid_request"}`，不排队不覆盖（server.ts:307-315）。
- **错误后连接处置**：响应已发出后再出错 → **杀连接**（无法保证流一致性，server.ts:387-390）；响应未发出 → 回错误响应、连接保留。`wrong_server` 只是普通错误响应，不断连（server.ts:346）。
- **订阅顺序保证**：subscribe 调用期间产生的增量先入 `pendingUpdates` 缓冲，**快照响应写出去之后**按序 flush——wire 上快照严格先于一切增量（server.ts:338-344, 375-382）；重复 subscriptionId → 拒（server.ts:347-349）；订阅失败但 encoder 已装 → 回滚删除（server.ts:384-386）。
- **Session 路由（SessionRouter）**：每个客户端连接对每个 session 拿到服务端签发的 `attachmentId = randomUUID()` 能力（session-router.ts:168）；调用时校验 `sessionId` 与 `attachmentId` 都等于当前 attachment，否则 `session_not_attached`（session-router.ts:224-232）；attach/detach 通过**带外 attachment 消息**发布新路由/undefined（session-router.ts:192-196, 254-260）；连接断开只等"已受理调用"落地再释放 attachment（session-router.ts:234-252；README:13 "after admitted calls settle"）。
- **应用注入点**：`ServerHost` 接口要求宿主提供 `serverServices`（server 级服务）+ `resolveSession` + `openSession`（types.ts:59-64）——Session/AgentHarness 留在应用进程内，server 包只拿 opaque handle。

### 2.4 client 侧行为事实（`packages/client/src/`）

- **身份 pin**：客户端构造时**声明期望的 serverId**，握手回包 serverId 不符 → 连接失败（connection.ts:174-181）——防止连到陈旧 socket 路径背后的"错误服务器"。
- **响应关联纪律（不对称设计）**：收到无匹配请求的 response → **杀连接**（"Response has no matching request"，client.ts:333-336，协议腐坏信号）；收到未知 subscriptionId 的 service_update → **静默丢弃**（client.ts:314-315，退订竞态属正常）。
- **attachment 校验**：带外 attachment 更新若 serverId 不符 → 杀连接（client.ts:306-308）。
- **两阶段队列**：订阅增量先缓存原始 wire 值（`queuedWireUpdates`），快照解码水合后逐个 decode，再等消费方 `start()` 才投递——保证监听器永远只见"快照+后续"的一致序列（client.ts:180-236, 313-331）。
- **退订新鲜度**：unsubscribe 只在"仍连接且 target 仍是当前 attachment"时发送（`#targetIsCurrent`，client.ts:226-227, 423-431）——过期退订不发。
- **时序防呆**：服务端数据早于客户端 hello 发出 → 判协议错误（connection.ts:146-149）。

### 2.5 chord 语法面（`packages/chord/src/services/wire.ts`，只看对外语义）

- **控制词汇**：`serviceId = "$chord.service"`，三个 member——`catalogue()`（无参）、`subscribe(subscriptionId, serviceId, mode)`（mode ∈ `singleton|keyed`）、`unsubscribe(subscriptionId)`；控制调用不带 instance（wire.ts:39-87）。
- **ServiceCall**：`{serviceId, member, args[], instance?}`，**键白名单**（未知键拒绝，wire.ts:89-97, 212-222）；instance 地址 `{key, generation≥1}`（wire.ts:197-203）。
- **catalogue**：`[{serviceId, mode}]` 数组，serviceId 去重（wire.ts:99-111）。
- **快照/增量**：快照 `{serviceId, mode, instances:[{instance?, members:[method|state(sequence,ops)]}]}`；增量五种类型 `state(member,sequence,ops) / unavailable / replaced(snapshot) / spawned(instance) / closed(instance)`（wire.ts:11-37, 133-195）；`sequence` 单调（state 快照从 0、增量从 1 起）。
- **delta path codec**：首个 flush 必为完整 base batch，后续按路径增量；字符串保留 append/front-truncate 语义、数组保留 append；每个订阅独立字典，replacement/不可用/重水合时重置（chord README:81-87, 105-123）。
- **服务错误码**（8 个）：`service_not_allowed / service_not_found / service_mode_mismatch / service_member_not_found / service_member_mismatch / service_instance_not_found / service_stale_instance / service_invalid_value`（services/errors.ts:2-11）。

### 2.6 测试名即契约（protocol/test/）

`protocol.test.ts` 契约清单：版本 8 严格相等；客户端 hello 接受任意整数 version（7/8/9 都能 parse——协商在 `isSupportedProtocolVersion`）；非规范 UUIDv4 五种变体全拒（大写/错 version 位/错 variant 位）；opaque payload 拒绝 byte array/NaN/undefined 属性/循环；cancel 空 id 与多余字段拒；attachment 更新接受三元组与 null；空 request id / 多余信封字段拒；void 响应（无 result 字段）合法；任意非空错误码接受；未知消息类型与未知字段拒；JSON 字符串不是消息；出站帧上限在编码时强制；分片/合包在**每一个切分点**都正确解码；坏帧后解码器毒丸化；截断流在 `end()` 抛错；超限帧头即拒。

`framing.test.ts` 契约清单：4 字节大端头；分片/合包/空帧按序；跨多 block 组装；每个切分点；不 alias 输入；截断 end 抛错；超限头即拒；恰好等于上限接受；end 后 push 拒；非法 maxFrameLength 抛 RangeError。

### 2.7 pi 的局限（README 自述）

- 整个协议 **experimental，无兼容性保证**（README:41）；应用侧也确实只放在 devDependencies + experimental 目录。
- **peer 认证与认证服务上下文未实现**（README:39）——ION 的 peercred 同 uid 校验（274d909 批次）在这一点上已**超前于 pi**。
- server/worker 生命周期刻意排除在公共协议外——每个 server 进程自有私有生命周期协议（README:19）。

---

## 三、ION 已有能力

| 能力 | ION 实现位置 | 状态 |
|------|-------------|------|
| per-session epoch 路由栅栏（重绑推进 + `stale_route` 作废旧订阅） | `src/bin/ion.rs`（SessionRouter + epoch 表）+ [SUBSCRIBE_PROTOCOL.md §1.1](./SUBSCRIBE_PROTOCOL.md) | ✅ master@274d909 |
| 快照先行（subscribe 后、replay/增量前推 `snapshot` 帧） | 同上 §1.2 | ✅ |
| hello 握手（可选，回 `protocolVersion:1`，不消费连接） | 同上 §1.3 | ✅ |
| ui_respond 同源绑定（需同连接先 `subscribe {ui:true}`） | 同上 §1.4 | ✅ |
| socket peercred 同 uid 校验（fail-closed） | 274d909 安全加固批次（SECURITY_HARDENING.md） | ✅（pi 明确不做，ION 超前） |
| host↔worker 私有 JSONL RPC（~120 命令，生命周期协议不外暴） | `src/worker_rpc.rs` | ✅（对应 pi "server process owns the private lifecycle protocol"） |
| `rpc_response` 通用事件兜底（每条用户 RPC 广播摘要） | `src/worker_rpc.rs`（output_response/output_error_response 汇聚点） | ✅ |

---

## 四、ION↔pi 映射表

| # | pi 概念 | pi 证据 | ION 对应物 | 评估 |
|---|--------|---------|-----------|------|
| 1 | `attachmentId` 三元路由栅栏（per-connection 能力句柄，服务端签发 UUID，attach/detach 带外更新，**连接不断**） | protocol.ts:40-45; session-router.ts:168, 192-196 | per-session **epoch**（host 级计数器，重绑推进，`stale_route` + **断开**旧订阅） | 语义不同层：pi 是"每连接独立能力 + 热切换续用"；ION 是"整 session 代际 + 失效即断"。**当前安全**（ION 的重绑源是 worker 死亡——会话状态已失效，断开最诚实）。若未来做 worker 热切换/会话内换 worker 不断线，需重审（见 §6 简化清单第 2 条） |
| 2 | CBOR 严格子集 + 4 字节大端帧 + 16MiB 上限 | framing.ts:6, 28-39; cbor/options.ts:3-8 | JSON 行（JSONL over Unix socket），**无行长上限** | 上限缺失是**真实缺口**（P0.1）；编码格式本身 avoid（§5.4） |
| 3 | 信封未知字段拒绝（`additionalProperties:false`） | protocol.ts:9-10 | 宽容解析（新字段/新帧旧端忽略） | ION 的宽容是**文档化的演进策略**（SUBSCRIBE_PROTOCOL.md §2：epoch/snapshot 都是增量字段加出来的），保留（§5.4 avoid 第 2 条） |
| 4 | `ServerHello.serverId` 逻辑身份 + 客户端 pin 校验 | protocol.ts:65-69; client connection.ts:174-181 | socket 路径即身份（`ION_HOST_SOCKET`），**无逻辑实例标识** | **缺口**：陈旧/异 workspace socket 会静默连错。P0.3 用 hello 加 `hostId` 低成本补齐 |
| 5 | 版本握手强校验（首条必须 hello；不符 → `hello_error` 杀连接） | server.ts:224-269 | hello 可选、不校验版本、不消费连接 | 有意简化：ION host/worker 永远同二进制，版本漂移只在外部 UI 客户端；hostId pin（P0.3）比版本击杀更对症。**安全** |
| 6 | opaque payload 纪律（协议层只验 strict-JSON，语义归 chord adapter） | codec.ts:20-32 | ION 事件直接携带应用数据（extension_event data），无独立语法层 | **安全**：ION 是单体内核，host/worker/webui 同团队同仓；无跨组织边界需要"信封不懂 payload"。serde 强类型已承担 pi 靠 strict-JSON 防的腐坏 |
| 7 | 订阅快照-增量顺序保证（server 端 pendingUpdates 缓冲） | server.ts:338-344, 375-382 | router 独占 rx + epoch 盖章 + snapshot 帧先推 | **等价达成**，实现更简单 |
| 8 | 客户端两阶段队列（hydrated/ready） | client.ts:180-236 | webui 侧"刷新恢复已完成只信 get_messages"铁律（ion-webui AGENTS.md） | 等价目标，不同机制，无需动 |
| 9 | 重复请求 id 拒绝 / cancel 同 id 同 target 命中 / 无匹配 response 杀连接 | server.ts:298-315; client.ts:333-336 | ION socket 是行式 req/resp + 广播摘要事件，无多路复用 pending 表 | 概念上无对应；`rpc_response` 广播只带摘要不带关联 id 语义。**安全**（无关联可腐坏）。若未来做多路复用 RPC，再引入 pi 的 pending 纪律 |
| 10 | 未知 subscriptionId 的 update 静默丢 / 退订新鲜度检查 | client.ts:314-315, 226-227 | 无客户端自选订阅 id（订阅绑定连接） | 简化安全；若加订阅 id，须同时抄 server.ts:347-349 的重复 id 拒绝 |
| 11 | 错误码模型：形状固定（非空 code + message）、内部错误脱敏为 `internal_error` + 固定文案、消息 500 字符截断 | server.ts:512-521; codec.ts:34-37 | rpc 错误字符串直通（无统一脱敏/截断） | **缺口**：内部路径/细节可能跨 socket 泄漏，超长错误无界。P0.2 |
| 12 | chord `$chord.service` 控制词汇 + delta path codec | wire.ts:39-87 | 无对应（subscribe/ui 流为内建语法，无服务发现/复制品状态） | 无需对齐：ION 无"服务组合运行时"诉求；LLM 流式粒度已是 token 级 text_delta，协议层 delta 压缩冗余（§5.4 avoid 第 3 条）。唯一远期场景：大状态树（文件树/待办列表）向 UI 复制时再评估 |
| 13 | peer auth 不做（README:39） | — | peercred 同 uid 已做 | ION 超前，无需动作 |
| 14 | 5s 握手超时 | server.ts:42 | 无首行超时（静默连接占住任务） | 小缺口，P2.2 |

---

## 五、对齐方案（可执行建议）

### P0（低成本高收益，建议尽快做）

| # | 待对齐项 | pi 依据 | 实现方案（file 级落点） | 验证 |
|---|---------|---------|----------------------|------|
| P0.1 | **socket 行长度上限** | framing.ts:77-79（头部即拒）；framing.ts:6（16MiB） | `src/bin/ion.rs` host socket 读循环：按字节读行时设 `MAX_LINE_LEN = 16 MiB`，超限**立即**回一条错误帧并断连（不等整行缓冲完）——JSONL 版的"头部即拒" | `tests/socket_security_ci.sh` 加 case：发 17MB 无换行 → 连接被断且有错误帧 |
| P0.2 | **错误响应脱敏 + 有界** | server.ts:512-521（internal_error 脱敏）；codec.ts:34-37（500 字符截断） | `src/bin/ion.rs` socket RPC 错误路径：错误消息统一过 `bound_error_msg()`（500 字符截断）+ 非 `RpcError` 类内部错误映射为固定文案（原始错误进 tracing 日志） | `ion rpc` 触发一个内部错误 → 响应 error 字段无路径/无超长串；日志有全量 |
| P0.3 | **hello 增加 hostId（实例身份 pin）** | protocol.ts:65-69（ServerHello.serverId）；client connection.ts:174-181（pin 校验） | hello 响应 data 加 `"hostId": "<进程启动时 random 生成，内存态>"`（按存储落位原则：不持久化、不建文件）；ion-webui/CLI 连接后可比对，防连到陈旧/异 workspace host | `tests/subscribe_protocol_ci.sh`：hello 返回含 hostId；两次 hello 同 hostId；重启 host 后 hostId 变化 |

### P1（应该做，中等收益）

| # | 待对齐项 | pi 依据 | 实现方案 |
|---|---------|---------|---------|
| P1.1 | hello 支持客户端版本询问/失配显式提示 | server.ts:263-269（version 失配 → 明确错误码） | hello params 可带 `{minProtocolVersion}`；host 版本低于要求时回 `success:false, error:"version"`——旧客户端不发则零影响 |
| P1.2 | 订阅 id 纪律预案（文档化，暂不实现） | server.ts:347-349；client.ts:314 vs 333（不对称处置） | 在 SUBSCRIBE_PROTOCOL.md 补一节：若未来引入客户端自选 subscriptionId，必须同时实现重复 id 拒绝 + "未知 id 静默丢、无匹配响应断连"的不对称纪律 |
| P1.3 | 静默连接空闲超时 | server.ts:42（5s 握手超时） | socket accept 后 N 秒无首行 → 关闭（N 可取 60s，宽于 pi 的 5s，兼容 CLI 手工调试） |

### P2（可选，锦上添花/远期）

| # | 项 | 说明 |
|---|----|------|
| P2.1 | attachment 语义（连接不断、带外换路由） | 仅当 ION 做"worker 热切换/会话内无缝换 worker"时才需要；当前 worker 死亡=状态失效，断开重订是更诚实的语义 |
| P2.2 | 大状态复制（chord delta path codec 对标） | 仅当 UI 需要订阅大状态树（如整棵文件树、千条待办）且全量帧成为瓶颈时再评估；LLM 文本流不需要 |
| P2.3 | CBOR 双栈（JSONL 为主、CBOR 可选协商） | 仅当出现二进制大 payload（图片/音频事件）且 base64 开销实测成瓶颈时再评估 |

### 明确 avoid（及理由）

1. **CBOR + 4 字节二进制帧整体替换 JSONL**。理由：(a) ION 的 JSONL 是 CLI/`ion rpc`/shell 脚本/tests/webui 全生态的公共契约，可 grep 可 curl 可 python3 三行验证——这正是 AGENTS.md"命令行可验证原则"的地基，换二进制等于毁掉它；(b) ION 单帧负载（text_delta、事件 JSON）远小于 16MiB，CBOR 的紧凑收益趋近于零；(c) Rust 侧需自研/引入 CBOR 严格子集 + 模糊测试，成本高；(d) pi 用 CBOR 的动机是 TUI 级高频 delta 流 + 严格 JSON 子集防腐坏——ION 的防腐坏已由 serde 强类型承担。**P0.1 的行上限已拿到二进制帧方案 90% 的安全收益。**
2. **信封未知字段拒绝 + 强制 hello-first**。理由：ION 的宽容解析是文档化的演进策略（epoch/snapshot/stale_route 全是靠"旧端忽略新帧"无破坏上线）；pi 能严格是因为它有强制版本握手且协议本身 experimental。ION 若要严格性，P0.3 的 hostId pin + P1.1 的版本询问已覆盖真实风险（连错人、版本漂移），无需牺牲兼容纪律。
3. **chord 式服务组合层（$chord.service/catalogue/facet）**。理由：ION 的扩展系统（36 钩子 + 27 host functions + WASM ABI）已覆盖 chord 的编排诉求且是 extension-first 而非 service-token-first；引入服务目录/复制品状态是与现有 ExtensionRunner 平行的新抽象，收益不明确。

---

## 六、SUBSCRIBE_PROTOCOL.md 已实现部分 vs pi 语义 — 差异与简化安全性清单

| # | pi 语义 | ION 简化 | 简化是否安全 |
|---|--------|---------|-------------|
| 1 | hello **强制为首条消息**，版本不符杀连接（`hello_error`） | hello 可选、可反复、不消费连接、不校验 | ✅ 安全：host/worker 同二进制；外部客户端版本漂移风险由 P0.3 hostId pin 覆盖 |
| 2 | attachment 变更是**带外续用**：连接保持，路由原地切换，已受理调用落地后才释放 | 重绑 = `stale_route` + **断开**旧订阅信道，客户端拿新 epoch 重订 | ✅ 当前安全（重绑源是 worker 死亡，旧 worker 状态已不可续）；⚠️ 若做 worker 热切换需升级为 pi 式带外换路由（P2.1），届时 epoch 可保留作代际戳 |
| 3 | 快照是**服务级**可复刻状态（chord snapshot + delta ops + sequence） | 快照是**应用级聚合**（worker/session/pendingApprovals/genAt） | ✅ 安全：不同层的东西；ION 无复刻状态诉求，聚合快照正中"新终端不空白"目标 |
| 4 | 订阅增量与快照的顺序由 **server 缓冲**保证 | 由 **router 独占 rx + 先推 snapshot 帧再放行增量**保证 | ✅ 等价保证 |
| 5 | 单订阅快照水合失败 → 连接级腐坏处置（杀连接） | 无对应（快照组装失败即无快照帧） | ✅ 安全：ION 快照是 host 直读内存/索引，失败面小；但可考虑快照组装失败时回一条错误帧而非静默（小改进，未列级） |
| 6 | 客户端两阶段队列（hydrated/ready） | webui 靠"已完成只信 get_messages"恢复铁律 | ✅ 等价目标不同机制 |
| 7 | 错误信封 `{code,message}`、内部错误脱敏、消息有界 | ION 错误字符串直通 | ❌ **不安全** → P0.2 |
| 8 | 帧上限 16MiB、头部即拒 | JSONL 无行长上限 | ❌ **不安全** → P0.1 |
| 9 | 连接有逻辑身份（serverId pin） | socket 路径即身份 | ⚠️ 有缺口（连错 host 静默发生）→ P0.3 |
| 10 | peer auth 不做 | peercred 同 uid fail-closed | ✅ ION 超前，无动作 |
| 11 | epoch 表 host 进程内、重启从 1 重计（SUBSCRIBE_PROTOCOL.md §5.3 自认） | pi 无对应物（attachmentId 是 UUID，天然无跨重启语义） | ✅ 安全：重启后 attachment/epoch 类身份本就该全部失效；hostId（P0.3）恰好为客户端提供"重启检测"信号 |

---

## 七、ION 原创设计（pi 没有）

| 能力 | ION 设计 | 原因 |
|------|---------|------|
| 审批同源绑定 | `ui_respond` 必须在已 `subscribe {ui:true}` 的**同一连接**上应答（SUBSCRIBE_PROTOCOL.md §1.4） | pi 明确不做认证/鉴权（README:39）；ION 的审批是高危操作（放行写文件/命令），同源绑定是最小可信防线 |
| socket peercred 同 uid 校验 | accept 时 SO_PEERCRED（fallback local uid）校验，fail-closed | 同上，ION 面向本地多用户场景的默认安全态（SECURITY_HARDENING.md） |
| `rpc_response` 通用广播兜底 | 每条用户 RPC 自动广播摘要事件，新 RPC 天然多终端可观测 | pi 无此机制；ION 依"被动通知+Pull+多窗口同步"三能力规范（AGENTS.md UI 交互架构规范） |
| session 级订阅 router（懒创建、至多一个） | 多连接订阅同 session 共享一个 SessionRouter，重绑只推进一次 epoch | pi 是每连接独立 attachment；ION 的多终端同步场景决定了 per-session 扇出更省 |

---

## 八、实施进度

### 已完成

- ✅ pi protocol/chord/server/client 包深读（upstream/main@f9bcd351d），逐文件事实清单成文（2026-09-15，本文档）

### P0 落地进度

- ✅ P0.2 错误响应脱敏 + 有界（G4 分支落地：`ion-protocol::sanitize_error` 收口**全部** error 构造点——`worker_response::error` / `host_response::error` / `invalid_json` / `stream_error_frame` / `rpc_response_event`；panic/backtrace 细节收敛固定文案 + `$HOME` 路径 `~` 化 + 500 字符 char 截断；公共错误文案原样保留，特征测试 Unknown command 断言持续绿）
- ✅ P0.3 hello hostId 实例身份（G4 分支落地：`ion_protocol::host_id()` 进程内存态 UUIDv4（不落盘）+ `hello_reply` 携带 + schema/特征测试/`subscribe_protocol_ci.sh` G3 同步 + CLI `ION_EXPECT_HOST_ID` pin（不匹配报错断开））
- ⏳ P0.1 socket 行长度上限（`src/bin/ion.rs`）

### 参考

- [SUBSCRIBE_PROTOCOL.md](./SUBSCRIBE_PROTOCOL.md) — ION 订阅协议已实现部分
- [SECURITY_HARDENING.md](./SECURITY_HARDENING.md) — 本地默认态 P0（peercred / 受保护路径）
- pi 源码：`/Users/xuyingzhou/Project/temporary/pi-mono-upstream-main/packages/protocol/`（v0.85.1，协议版本 8）
