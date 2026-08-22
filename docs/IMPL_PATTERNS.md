# 实现规律与约束（通过 13 项补齐总结出的模式）

> 这些不是文档里写的规范，而是**从代码实践中提炼的潜规则**——违反就会翻车。

---

## 一、事件推送模式

### 规律：所有事件走同一条管道

```
Worker Extension → stdout JSON → Manager stdout-reader → EventBus → 所有 subscriber
```

### 约束

| # | 规则 | 违反后果 | 来源 |
|---|------|---------|------|
| 1 | 事件必须包 `"type":"event"` 外壳 | Manager 不转发，subscriber 收不到 | AGENTS.md 协议约束 |
| 2 | 事件用 `emit_public_event()` 发射（不手拼 JSON） | 手拼容易漏外壳/字段名不一致 | 本轮 13 项补齐总结 |
| 3 | `customType` 用 PascalCase（`FilesRestored` 不是 `files_restored`） | 前端匹配不到 | 与现有 `ApprovalRequest` 一致 |
| 4 | 事件 data 带 **足够 UI 渲染的信息**（不只是"变了"，要有"变成什么"） | UI 收到事件还得再拉一次数据 | 十六问 #3 |
| 5 | **写操作完成后**才发事件（不是发完事件再写） | 事件到了但数据还没落盘，UI 拉到旧数据 | 本轮实践 |

### 正确的事件发射

```rust
// ✅ 用公开 API，自动包 type:event 外壳
crate::file_snapshot::approval::emit_public_event(
    "SettingsChanged",              // PascalCase customType
    &serde_json::json!({
        "key": "permission_mode",   // 足够 UI 更新的数据
        "value": mode,
    }),
);
```

---

## 二、RPC 设计模式

### 规律：读写分离，一问一答

| # | 规则 | 例子 |
|---|------|------|
| 1 | **读操作不产生副作用** | `get_messages` 只读，不改状态 |
| 2 | **写操作成功后推事件** | `review_approve` → `ApprovalResolved` |
| 3 | **批量操作返回明细** | `approve_all` 返回 `failures[]`，不能只给汇总 |
| 4 | **参数校验先行**（missing 就 early return，不走到业务逻辑） | `if mode.is_empty() { return error }` |
| 5 | **列表类 RPC 要有 LIMIT 参数** | `get_messages {cursor}` / `memory list {limit}` |
| 6 | **响应不带全量内容**（除非明确请求） | `review_pending` 不带 oldContent/newContent（55MB 教训） |

### 潜规则

| # | 规则 | 为什么 |
|---|------|--------|
| 1 | host 级 RPC（跨 session）放 `bin/ion.rs`；worker 级 RPC（session 内）放 `worker_rpc.rs` | 架构分层 |
| 2 | 查询参数用 snake_case（`modelId` 除外——历史遗留） | 大多数一致，`modelId` 是坑 |
| 3 | `session` 参数在请求顶层（不在 params 里） | gateway 协议约定 |
| 4 | worker 冷启动期间 RPC 可能返回空壳 `{"status":"forwarded"}` | 需 UI 端容错 |
| 5 | **每个 RPC 都可通过 CLI 直调**：`ion rpc --method xxx --session yyy` | socket 统一暴露 |

---

## 三、扩展依赖模式

### 规律：能力归属决定了代码在哪

| 能力类型 | 代码位置 | 调用方式 | 例子 |
|---------|---------|---------|------|
| **内核** | `src/worker_rpc.rs` | 直接 RPC | prompt / abort / get_messages |
| **内置扩展** | `src/file_snapshot/` / `src/agent/` | 直接 RPC（内核已桥接） | review_pending / turn_changes |
| **运行时扩展** | `extensions/*.wasm` | `extension_rpc {extension, method}` | todo / stock |
| **单例扩展** | `src/global_memory*.rs` | `extension_rpc {extension:"global-memory"}` 仅 serve | save / search / forget |

### 潜规则

| # | 规则 | 违反后果 |
|---|------|---------|
| 1 | 新增内核 RPC **必须同时加到 bg 分支**（agent 运行中也能查） | agent 跑着时 RPC 超时 |
| 2 | `extension_rpc` 必须标注 extension 名 + method 名 | 前端不知道调谁 |
| 3 | global-memory **仅 serve 模式可用**，非 serve 返回 error | CLI 单次执行时调不到 |
| 4 | 内置扩展的启用/禁用由 `config.json extensions.X.enabled` 控制 | 关掉后 RPC 报 not enabled |

---

## 四、数据持久化模式

### 规律：什么数据放哪里

| 数据类型 | 存储位置 | 恢复时机 | 重启后 |
|---------|---------|---------|--------|
| 会话轨迹 | `{sid}.jsonl`（only-append） | worker 启动时重放 | ✅ 完整 |
| 审批决策 | session.jsonl custom entry | `restore_from_session()` | ✅ 完整 |
| 全局记忆 | `global-memory.db`（SQLite+FTS5） | 直接读 | ✅ 完整 |
| 内存态（队列/进程/gateway缓冲） | **不持久化** | — | ❌ 丢失 |

### 潜规则

| # | 规则 | 为什么 |
|---|------|--------|
| 1 | **审批状态持久化到 session.jsonl**（不放独立文件） | AGENTS.md 存储落位原则 |
| 2 | **缓存键 = `(tree_hash, gen)`**（不是比较整个 map） | 避免每次锁全表比较 |
| 3 | **写操作先落盘再推事件**（先 persist 再 emit） | 事件到达时数据已可查 |
| 4 | **内存态数据承诺"不保证恢复"**（队列/进程/gateway缓冲） | 设计如此，不是 bug |

---

## 五、UI 数据集成潜规则

| # | 规则 | 违反 UI 表现 |
|---|------|-------------|
| 1 | **无推送 = 常驻陈旧条**（"数据截至 HH:MM"） | 用户不知道数据旧了 |
| 2 | **列表类 RPC 不带全量内容**（只回摘要） | 55.8MB 响应卡死浏览器 |
| 3 | **在途操作必须禁用按钮**（模块级 Set 锁） | 用户狂点→重复操作 |
| 4 | **批量操作要能看失败明细**（failures[]） | 用户不知道哪些没成功 |
| 5 | **删除操作必须 confirm + 说明后果**（能否恢复） | 用户误删 |
| 6 | **错误要分类**（可重试 / 等host / 被GC / 报bug） | 用户不知道该干嘛 |
| 7 | **功能的主入口在消息流/输入区**（不在专门面板） | 用户找不到入口 |
| 8 | **配置改动的生效时机必须告知用户**（toast 提示） | 用户以为要重启 |
| 9 | **iframe 内的弹层被约束在设备视口内**（不逃逸到全局） | 弹窗覆盖全屏 |

---

## 六、性能模式

| # | 规律 | 实测数据 |
|---|------|---------|
| 1 | **树/hash 解析提升出循环**（不逐文件重复读） | 695 文件 10.2s → 0.47s |
| 2 | **查询结果按 `(key, gen)` 缓存** | 命中 96µs |
| 3 | **骨架屏代替白屏等待** | 感知性能提升 |
| 4 | **事件 payload 带足够数据**（避免收到事件后还要回拉） | message_end 带 usage 零额外请求 |
| 5 | **JSON 序列化注意 `</` 转义**（嵌入 `<script>` 时） | 不转义会提前截断 script 标签 |

---

## 七、架构潜规则

| # | 规则 | 为什么 |
|---|------|--------|
| 1 | **禁止新建独立 sidecar 文件**存会话派生数据 | AGENTS.md 存储落位原则 |
| 2 | **统一叫 Extension，禁止 Plugin** | 术语规范 |
| 3 | **内核要足够强大，扩展只做策略层的事** | 内核 vs 扩展方针 |
| 4 | **多终端共享的东西放索引/SessionIndex** | 同上 |
| 5 | **所有功能必须命令行可验证**（CI 脚本 / ion rpc） | 测试规范 |
| 6 | **iframe 是"设备边界"**——外层是设计画布，内层是应用本体 | 原型架构 |

---

## 八、本轮 13 项补齐的模式总结

| 模式 | 补齐项数 | 代码量 |
|------|---------|--------|
| **emit_public_event 一行事件** | 9 个事件 | 每处 5-8 行 |
| **响应体增强**（加字段） | 2 个（failures[] / LIMIT） | 每处 10-15 行 |
| **新 RPC 方法** | 1 个（search_sessions） | ~60 行 |
| **新 store 方法** | 1 个（unarchive） | 5 行 |
| **公开 API 提取** | 1 个（emit_public_event） | 10 行 |

**规律：大部分能力缺口都是"一行事件"级别的——用统一的 `emit_public_event` 就能解决。真正需要大量代码的只有新查询逻辑（如 search_sessions）。**
