# ION Web 中断复原 RPC 设计（list_interrupted / recover_tree）

> **状态：开发中（设计已完成，待实现）** — host 级两个 RPC，支撑 Web UI "中断后一键复原"。

---

## 1. 背景与定位

现有 AUTO-RECOVERY（`src/worker_registry.rs` §AUTO-RECOVERY）覆盖**单 worker 粒度**的异常死亡自动重派：

- 触发：stdout EOF / exit_code ≠ 0 / 心跳超时（Busy > 10min）
- 动作：`try_auto_respawn` → `generate_resume_prompt`（原始任务 + 最近 6 条对话 + 断点续作指令）→ 在原 host 上 `create_worker`
- 限制：仅远程 worker、3 次上限、被动触发（worker 死的那一刻）

ION Web 的场景是 **host 级中断**：用户关掉浏览器/断网/host 重启，回来后想知道"我的会话树里哪些子 worker 死了"，并一键把整棵树拉起来。这需要两个**主动查询+批量恢复**的 RPC：

| RPC | 作用 |
|-----|------|
| `list_interrupted` | 扫 SessionIndex，找出有 `parent_session` 血缘且 worker 已死/不在跑的子会话，返回中断会话树 |
| `recover_tree` | 以 `{sessionId}` 为根，按血缘 BFS 走整棵树，对死节点复用 AUTO-RECOVERY 上下文恢复逻辑 `create_worker`，返回重派清单 |

## 2. RPC 接口规格

### 2.1 `list_interrupted` — 扫描中断会话树

**请求：**

```bash
ion rpc --method list_interrupted \
  --params '{"sessionId":"sess-root-abc123"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `sessionId` | string | 可选 | 限定扫描某根会话的子树；缺省则扫全索引所有带血缘的树 |

**响应 JSON（成功）：**

```json
{
  "ok": true,
  "trees": [
    {
      "root": {"sessionId": "sess-root-abc123", "title": "重构任务", "status": "dead"},
      "nodes": [
        {"sessionId": "sess-c1", "parent": "sess-root-abc123", "title": "worker-1", "status": "dead",   "host": "win38", "respawnCount": 2, "lastActivity": 1789227000},
        {"sessionId": "sess-c2", "parent": "sess-root-abc123", "title": "worker-2", "status": "running", "host": "win38", "respawnCount": 0, "lastActivity": 1789228100}
      ],
      "deadCount": 1
    }
  ],
  "totalDead": 1
}
```

**节点 status 枚举：** `running` | `dead`（worker 不在 registry 或已 exit）| `local`（本地会话，无 worker 概念，只作树的中间节点）。

### 2.2 `recover_tree` — 一键复原

**请求：**

```bash
ion rpc --method recover_tree \
  --params '{"sessionId":"sess-root-abc123"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `sessionId` | string | 必填 | 树根（也可以传任意子节点——恢复以它为根的子树） |
| `maxConcurrentRespawns` | number | 3 | 同时 spawn 替补上限，防止多 worker 同时死时压垮远端（对齐 AUTO-RECOVERY 已知漏洞"多 worker 同时死"） |

**响应 JSON（成功）：**

```json
{
  "ok": true,
  "respawned": [
    {"sessionId": "sess-c1", "host": "win38", "newWorkerId": "w-8812", "resumePromptChars": 1204}
  ],
  "skipped": [
    {"sessionId": "sess-c2", "reason": "already_running"},
    {"sessionId": "sess-c3", "reason": "respawn_limit_reached"}
  ],
  "failed": [
    {"sessionId": "sess-c4", "reason": "ssh_unreachable", "detail": "connect timeout to win38"}
  ]
}
```

**事件：** 每个成功重派 emit `auto_recovered` UI 事件（复用现有事件，触发源加 `recover_tree`），Web UI 可 `subscribe` 实时看到节点逐个变绿。

## 3. SessionIndex 查询策略

遵循存储落位原则：**血缘（parent_session）天然是索引字段，查询只读 `sessions.index.json`，不新建任何文件**。

1. `SessionIndex::load()` 一次性载入内存（整文件 load/save，索引小可接受）。
2. 过滤：`meta.parent_session.is_some()` 的条目构成"子节点池"。
3. 存活判定：拿 `sessionId` 查 WorkerRegistry——在 registry 且未 exit → `running`；不在 registry（host 重启后 registry 是空的）→ `dead`。本地会话（无 host）→ `local`。
4. 分组：对每个子节点沿 `parent_session` 向上找到根，按根聚合成树；根不在索引/根死活未知时根节点 status 以 registry 判定为准。

## 4. 树遍历算法（recover_tree）

```
queue = BFS([sessionId])
while queue:
    sid = queue.pop()
    meta = index.get(sid)
    status = liveness(sid)
    if status == dead and is_remote_worker_session(sid):
        if respawn_count(sid) >= 3: skipped(limit_reached)
        else:
            prompt = generate_resume_prompt(session_jsonl_path(sid))   // 复用 AUTO-RECOVERY
            match create_worker(host=meta.workspace host, prompt):
                Ok(wid)  -> respawned += (sid, wid); emit auto_recovered
                Err(e)   -> failed += (sid, e)   // 单节点失败不中断整树
    else:
        skipped(already_running / local)
    queue.extend(index.children_of(sid))    // children_by_parent 过滤
```

关键点：**BFS 自上而下**——父节点先恢复（它可能持 worktree/锁），子节点后恢复；重派信号量限制 `maxConcurrentRespawns`；单个节点失败只记入 `failed`，继续走剩余节点（部分失败容忍）。

## 5. 边界情况

| # | 情况 | 处理 |
|---|------|------|
| 1 | 节点在跑（running） | `skipped(already_running)`——幂等：重复调 recover_tree 不会重复 spawn |
| 2 | 本地会话节点 | `skipped(local)`，只作为树的通路继续向下遍历 |
| 3 | respawn 次数 ≥ 3 | `skipped(respawn_limit_reached)`，与 AUTO-RECOVERY 上限一致，防死���环 |
| 4 | 部分节点 SSH 不可达 | 记入 `failed`，不中断整树；响应里区分 ssh_unreachable 供 UI 提示"远端宕机" |
| 5 | resume prompt 生成失败（JSONL < 3 行 / 无用户任务） | 该节点记 `failed(no_resume_context)`，跳过 |
| 6 | 重复调用（幂等） | 第二次调用时已恢复的节点是 running → skipped；天然幂等 |
| 7 | 树成环（脏数据） | BFS 用 visited 集合防环 |
| 8 | sessionId 是叶子不是根 | 合法——恢复以它为根的（单节点）子树 |
| 9 | 父死子活 | 父被重派成新 worker；子仍在跑不动——父子逻辑一致性靠 resume prompt 中上下文兜底（与现有 AUTO-RECOVERY 同水平） |

## 6. CLI 验证用例（Group A/B/C/D）

### Group A — 正常路径

- **A1** spawn 一个远程 worker（子会话）→ pkill 杀掉 → `ion rpc --method list_interrupted`，断言返回树含该节点 `status:"dead"`。
- **A2** `ion rpc --method recover_tree --params '{"sessionId":"<root>"}'`，断言 `respawned` 含该 sid 且 `newWorkerId` 非空。
- **A3** `ion subscribe` 监听，断言收到 `auto_recovered` 事件。

### Group B — 幂等与跳过

- **B1** 紧接 A2 再调一次 recover_tree，断言该节点进 `skipped(already_running)`。
- **B2** 本地子会话（无 host）在树中 → `skipped(local)`。

### Group C — 边界

- **C1** 连杀 3 次并 recover 3 次 → 第 4 次 `skipped(respawn_limit_reached)`。
- **C2** 断开 win38 SSH → recover_tree → 该节点进 `failed(ssh_unreachable)`，其余节点不受影响。
- **C3** 传叶子 sessionId → 单节点子树可恢复。

### Group D — e2e（真机）

- **D1** `ION_E2E=1 cargo test --test remote_worker_ci` 场景扩展：杀整树 → list_interrupted → recover_tree → 替补收到含"自动恢复：断点续作"的 prompt 并继续任务。

## 7. 实现落点（指引）

- RPC 注册：`src/bin/ion.rs` 的 method 分发表（参照 `mcp_restart_server` 等现有 method）。
- 逻辑主体：`src/worker_registry.rs`，紧邻 `try_auto_respawn` / `generate_resume_prompt`，直接复用。
- 索引查询：`SessionIndex::children_by_parent` / `set_parent` 相关既有 API。
- CI 脚本：`tests/web_recovery_ci.sh`（起 host → 造树 → 杀 → 断言）。
