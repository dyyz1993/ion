# FIX: `--host --export` 组合不生效（场景二无法自动导出 HTML）

> **状态：开发中** — spec 已写好，待 developer 实现。
> **类型：bug fix**（A→B 流程，ZCode 写 spec，developer 改代码）

## 1. 问题描述

`ion --host --export <path> "prompt"` 一起用时，**HTML 永远不会自动生成**。

### 复现步骤

```bash
cd /tmp/clean-dir
ion --host --agent developer --export /tmp/out.html "说一句话"
# worker 跑完，host cleanup complete，但 /tmp/out.html 不存在
```

### 根因

`src/bin/ion.rs` 第 ~3953 行：

```rust
// export_after_run 在前面已经正确计算出来了（因为 cli.host=true → has_run_intent=true）
let export_after_run: Option<String> = if let Some(ref export_path) = cli.export {
    let has_run_intent = !eff.message.is_empty() || cli.host;  // ← true
    if has_run_intent { Some(export_path.clone()) } else { ... }
} else { None };

if cli.host {
    let msg = ...;
    cmd_host(&msg, cli.agent.as_deref()).await;   // ← ❌ 没传 export_after_run
    return;                                       // ← 直接 return，export 永远丢失
}
```

`cmd_host` 函数签名（第 ~5440 行）**没有 export 参数**：

```rust
async fn cmd_host(user_message: &str, agent_name: Option<&str>) { ... }
```

所以 export_after_run 算了等于白算，`--host` 路径走完直接 return，HTML 从不生成。

## 2. 修复方案

### 2.1 改 `cmd_host` 签名（src/bin/ion.rs ~5440）

加一个 `export_path: Option<&str>` 参数：

```rust
async fn cmd_host(
    user_message: &str,
    agent_name: Option<&str>,
    export_path: Option<&str>,   // ← 新增
) {
```

### 2.2 在 cleanup 完成后做 export（src/bin/ion.rs ~5697，函数末尾）

现在函数末尾是：

```rust
    // 给 Worker 时间执行退出前 save_worker_session
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    eprintln!("[host] cleanup complete");
}  // ← 函数结束
```

改成（在 `}` 前插入 export 逻辑）：

```rust
    // 给 Worker 时间执行退出前 save_worker_session
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    eprintln!("[host] cleanup complete");

    // ── Export after host run (if --export was given) ──
    // 对齐 cmd_run 的 export-after-run 行为：host 跑完后导出 entry worker 的 session HTML。
    if let Some(path) = export_path {
        match ion::export::export_session_rich(&entry.session_id, std::path::Path::new(path)) {
            Ok(()) => println!("Exported to {path}"),
            Err(e) => eprintln!("Export failed: {e}"),
        }
    }
}
```

**说明**：
- `entry` 是 entry worker 的 `WorkerInfo`（第 ~5605 行 `let entry = { ... };` 捕获），其 `session_id: String` 字段就是要导出的 session。
- 用 `export_session_rich`（不是 `export_session_with_tools_and_prompt`），因为 host 模式下 tools/system_prompt 快照在 worker 子进程里，主进程拿不到；`export_session_rich` 会从 agent config 重建 tools 列表（见 `src/export.rs:88`），足够用。
- export 放在 cleanup **之后**，确保 worker 的 save_worker_session 已落盘，export 能读到完整 session。

### 2.3 改 caller（src/bin/ion.rs ~3953）

```rust
    if cli.host {
        let msg = if effective_message.is_empty() {
            "Hello".to_string()
        } else {
            effective_message
        };
        cmd_host(
            &msg,
            cli.agent.as_deref(),
            export_after_run.as_deref(),   // ← 新增：把 export 路径传下去
        )
        .await;
        return;
    }
```

## 3. 验证方法（必须用命令行验证）

### 3.1 编译

```bash
cd /Users/xuyingzhou/Project/study-rust/ion
cargo build 2>&1 | tail -5
# 期望：无 warning，无 error
```

### 3.2 功能验证（场景二自动 export）

```bash
# 用临时 cwd，确保独立 session 文件
WORKDIR=$(mktemp -d)
cd "$WORKDIR"
ION_BIN=/Users/xuyingzhou/Project/study-rust/ion/target/debug/ion

# 跑场景二 + export，短 grace period 快速退出
ION_HOST_IDLE_GRACE=3 "$ION_BIN" --host --agent developer --profile permissive \
  --export /tmp/host_export_test.html \
  "回复一句：export-after-host 测试成功" \
  2>&1 | tail -5

# 期望最后输出：Exported to /tmp/host_export_test.html
ls -lh /tmp/host_export_test.html
# 期望：文件存在，size > 0
```

### 3.3 回归验证（standalone export 不受影响）

```bash
# 不带 prompt 的纯 export 仍应工作（走另一条分支）
SID=70973e3b-af36-40bf-9e4a-bdffd3ff0f0e  # 任意已有 session
"$ION_BIN" --export /tmp/standalone_test.html --session "$SID" 2>&1 | tail -2
# 期望：Exported to /tmp/standalone_test.html
```

### 3.4 单元测试（可选，加分）

如果方便，加一个测试确认 `cmd_host` 能接收 `Some(path)` 不 panic。但这个函数是 async + 涉及进程 spawn，单元测试难做，**以命令行验证（3.2）为准即可**。

## 4. 注意事项

1. **ALL COMMENTS MUST BE IN ENGLISH ONLY**（避免 U+FFFD 乱码问题）
2. **不要改 export_session_rich / export_session_with_tools_and_prompt 的实现**，只改调用方
3. **不要动 cmd_host 的其他逻辑**（idle 检测、cleanup 流程不变），只在末尾加 export 块
4. 改动范围：**仅 `src/bin/ion.rs`**（3 处：函数签名、函数末尾、caller）
5. `entry` 变量在函数内已经 move 进了 `set_entry_worker`？**需要确认**——如果 `entry` 被 move 了，在末尾用不了，需要先 clone session_id。看代码：

```rust
let entry = { ... };                                    // entry 在这
registry.lock().await.set_entry_worker(&entry.worker_id);  // 借用，没 move
```

`set_entry_worker(&entry.worker_id)` 是借用，`entry` 没被 move。但后面要确认 `entry` 在 wait loop 里有没有被 move/drop。**如果编译报 borrow/move 错误，就在 `let entry = {...};` 之后立刻 `let entry_session_id = entry.session_id.clone();` 缓存一份，export 用缓存的。**

## 5. 改动清单（checklist）

- [ ] `cmd_host` 签名加 `export_path: Option<&str>`
- [ ] `cmd_host` 末尾（cleanup complete 之后）加 export 块
- [ ] caller（`if cli.host {` 分支）传 `export_after_run.as_deref()`
- [ ] `cargo build` 无 warning/error
- [ ] 命令行验证 3.2 通过（HTML 自动生成）
- [ ] 命令行验证 3.3 通过（standalone 不回归）
