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

### 1. Runtime trait 使用默认实现 + 重写 ✅ 已定

Runtime trait 有约 20 个方法。RemoteRuntime 重写了 execute_command/read_file/write_file 等 15 个方法，其余通过 `self.inner.xxx()` 透传。SandboxRuntime 只重写了 execute_command（用 sandbox-exec 包装），其他透传。

**状态：已接受当前模式。** read/write 透传给 inner 是预期行为——砂箱只约束 `execute_command`（bash 命令），`read_file`/`write_file` 等工具调用走 SecuredRuntime 的 PermissionEngine 控制。两者分属不同防御层级。

### 2. Arc<Mutex<ExtensionEventBus>> 作为全局共享状态 🟡 待优化

SecuredRuntime 持有 `Option<Arc<Mutex<ExtensionEventBus>>>`，resolve_ask 时通过它推送 Ask 事件。Manager 的 socket handler 也持有同一个 EventBus，用于 subscribe --ui。

**状态：暂缓。** 当前并发量下 Arc+Mutex 足够。未来如果出现高并发 Ask 推送可改为 tokio broadcast channel。

### 3. RemoteRuntime 的 ProxyJump 实现 🟡 暂维持

ProxyJump 直接在 SSH 命令字符串里拼 `-J proxy_host`。没有用 libssh 或 ssh2 crate。

**状态：暂维持。** 字符串拼凑方式经实测可用（RemoteRuntime E2E 测试通过）。若未来需要更复杂的 SSH 管理（多 hop、密钥管理）再考虑 libssh。

### 4. SandboxRuntime 的 profile 文件写 /tmp ✅ 已修复

之前每次 execute_command 生成 `.sb` 文件到 `/tmp/` 不清理。

**状态：已修复（2026-07-07）。** 改用 sandbox-exec `-p` 内联 profile，不写任何临时文件。

### 5. SecuredRuntime 和 PermissionExtension 双重检查 🟡 待设计

PermissionExtension（before_tool_call 钩子）先检查自有规则表 → 放行给 SecuredRuntime（PermissionEngine + CommandGuard）。两条路径可能重复检查。

**状态：待设计。** PermissionExtension 是 Extension 钩子层，SecuredRuntime 是内核层，职责明确。但双重检查可能影响性能。后续当有真实用例时再优化。

### 6. 测试脚本依赖 Manager 进程 🟡 待优化

3 个 CI 脚本（runtime_ci / permission_ci / session_ci）都需要 `ion manager start` + `create_worker`，启动慢（~15s），且可能残留进程。

**状态：待优化。** 当前 apple_container_ci.sh 用了相同模式，但加了更完善的 cleanup trap。后续可考虑 mock Worker 加速单测。

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
