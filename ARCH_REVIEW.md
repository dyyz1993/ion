# ION 架构实现总结 — 求 review

## 项目定位

ION 是一个 Rust 实现的 AI Agent 编排平台，对标 pi (pi-coding-agent) 的能力。内核负责进程管理、通信、安全模型；策略/扩展通过 Extension trait 实现。

## 核心架构

### 三层 Runtime

```
config.json → runtime.default_mode
  ├── "local"   → LocalRuntime              ← 默认：本机执行
  ├── "remote"  → RemoteRuntime<Local>      ← SSH 远程执行
  ├── "sandbox" → SandboxRuntime<Local>     ← macOS sandbox-exec
  └── routes[]  → RouterRuntime             ← 命令级混合路由
        ↓
  SecuredRuntime（统一安全层）
        ↓
  WorkerRuntime（Worker 编排）
```

### 三种 Runtime 实现

**LocalRuntime**（~100 行）：直接 tokio::process / tokio::fs，无沙箱。

**RemoteRuntime**（~150 行）：包装内层 Runtime（通常是 Local），所有操作通过 `ssh user@host -p port 'command'` 转发。支持 ProxyJump（`-J bastion`）。文件读写用远程命令模拟（`cat > path` 写，`cat path` 读）。

```rust
// 关键设计：RemoteRuntime<R: Runtime> 泛型包装
// 这样 SecuredRuntime<RemoteRuntime<LocalRuntime>> 自动获得安全层
pub struct RemoteRuntime<R: Runtime> {
    inner: R,
    host_user, host_hostname, host_port, host_key, host_proxy_jump: String,
}
```

**SandboxRuntime**（~100 行）：自动生成 macOS Seatbelt `.sb` profile，用 `sandbox-exec -f profile.sb` 包装命令。三种内置 profile：readonly（只读+禁网）、workspace（工作区可写）、full-access（全部放行）。

**RouterRuntime**（~60 行）：根据 `runtime.routes` 配置，`tool + pattern` 匹配后路由到对应 Runtime。

**SecuredRuntime**（中间件，~250 行）：在 Runtime trait 方法上加 CommandGuard + PermissionEngine 拦截。`resolve_ask` 支持三条路径：同步 confirm_handler、异步 UI 通道 Ask、默认拒绝。

### 测试覆盖

| 套件 | 数量 | 方式 |
|------|------|------|
| 单元测试（lib） | 101 | Rust `#[test]` |
| 集成测试（integration） | 44 | `#[tokio::test]` |
| CLI 脚本（CI） | 48 | bash + `ion rpc` + `sandbox-exec` |
| E2E 远程（shanbox） | 8 | `ssh -J shanbox nas 'cmd'` |
| **总计** | **201** | |

### 验证结果

```
Phase 1: cargo test --lib               101 ✅
Phase 2: runtime_ci.sh (Local/沙箱/组合)  15 ✅
Phase 3: session_entries_ci.sh (RPC)     21 ✅
Phase 4: permission_ci.sh (权限规则)       12 ✅
Phase 5: RemoteRuntime via shanbox       8/8 ✅
Phase 6: sandbox-exec (macOS)           3/3 ✅
P0 修复后全量验证                        145 ✅ (2026-07-06)
```

## 关键设计决策 & 求 review 的点

### P0 已修复（2026-07-06）

| # | 问题 | 修复内容 | 状态 |
|---|------|---------|------|
| 1 | RemoteRuntime 透传 inner | spawn_process/kill_process/send_stdin 返回 Err | ✅ |
| 2 | SandboxRuntime 只包 execute_command | 文档明确说明；文件操作由 PermissionEngine 控制 | ✅ |
| 3 | SandboxRuntime profile 写 /tmp | 改用 `sandbox-exec -p` 内联 | ✅ |
| 4 | SSH 字符串拼接 | 路径统一经 `sh_quote()` 处理 | ✅ |

### 待讨论的 P1 设计问题

### 1. Runtime trait 使用默认实现 + 重写

Runtime trait 有约 20 个方法。RemoteRuntime 重写了 execute_command/read_file/write_file 等 15 个方法，其余通过 `self.inner.xxx()` 透传。SandboxRuntime 只重写了 execute_command（用 sandbox-exec 包装），其他透传。

**问：** 这样"只重写需要的"模式是否合适？SandboxRuntime 的 read/write 透传给 inner 是否会在沙箱外执行？

### 2. Arc<Mutex<ExtensionEventBus>> 作为全局共享状态

SecuredRuntime 持有 `Option<Arc<Mutex<ExtensionEventBus>>>`，resolve_ask 时通过它推送 Ask 事件。Manager 的 socket handler 也持有同一个 EventBus，用于 subscribe --ui。

**问：** 这种 Arc+Mutex 共享 EventBus 的方式在大并发下有没有问题？有没有更好的选择（比如 broadcast channel）？

### 3. RemoteRuntime 的 ProxyJump 实现

ProxyJump 直接在 SSH 命令字符串里拼 `-J proxy_host`。没有用 libssh 或 ssh2 crate。

**问：** 字符串拼凑的方式够用吗？是否应该用专门的 SSH crate？

### 4. SandboxRuntime 的 profile 文件写 /tmp

每次 execute_command 都生成一个 `.sb` 文件到 `/tmp/`，不主动清理。

**问：** 是否有更好的方式（比如 sandbox-exec -p 内联 profile）？不清理文件是否可接受？

### 5. SecuredRuntime 和 PermissionExtension 双重检查

PermissionExtension（before_tool_call 钩子）先检查自有规则表 → 放行给 SecuredRuntime（PermissionEngine + CommandGuard）。两条路径可能重复检查。

**问：** 这种分层是否合理？有没有更好的方式让 Extension 直接绕过 SecuredRuntime？

### 6. 测试脚本依赖 Manager 进程

3 个 CI 脚本（runtime_ci / permission_ci / session_ci）都需要 `ion manager start` + `create_worker`，启动慢（~15s），且可能残留进程。

**问：** 有没有更好的测试方式？比如 mock Worker 来加速？

## 快速开始

```bash
git clone ...
cd ion

# 运行全部测试
cargo test --lib
bash tests/runtime_ci.sh
bash tests/permission_ci.sh
bash tests/session_entries_ci.sh

# 远程执行（需要 SSH 跳板）
# ~/.ion/config.json:
# { "runtime": { "default_mode": "remote", "remote": { "default_host": "host", "hosts": {"host":{}} } } }
ion manager start
ion rpc --method create_worker --params '{"cwd":"/tmp"}'
ion rpc --session <sid> --method call_tool --params '{"tool":"bash","args":{"command":"echo hello"}}'
```
