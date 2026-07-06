# ION 部署架构 — 矩阵

## 可组合的维度

不是"一种模式"选完所有组件。**每个维度独立选择，自由组合：**

```
维度 A：Worker 在哪里？            Local │ Remote (SSH/WS)
维度 B：Runtime 是什么？           Local │ Remote │ Sandbox
维度 C：沙箱用在哪？              全部 │ 按命令选择 │ 不用
维度 D：Extension 在哪跑？        Worker 侧 │ Manager 侧
维度 E：会话数据存哪？             Worker 侧 │ Manager 侧
```

---

## 完整矩阵

| 组件 | Local Worker + Local Runtime（当前） | Local Worker + Sandbox Runtime | Local Worker + Remote Runtime | Remote Worker + (任意 Runtime) |
|------|-----------------------------------|-------------------------------|------------------------------|-------------------------------|
| **LLM 调用** | Worker 侧 | Worker 侧 | Worker 侧 | **远程 Worker 侧** |
| **Extension 执行** | Worker 侧（本地 wasmtime） | Worker 侧 | Worker 侧 | **远程 Worker 侧** |
| **Extension 数据** | Worker 侧 `~/.ion/` | Worker 侧 | Worker 侧 | **远程 Worker 侧** |
| **会话数据** | Worker 侧 `session.jsonl` | Worker 侧 | Worker 侧 | **远程 Worker 侧** |
| **Skills** | Worker 侧 `~/.ion/skills/` | Worker 侧 | Worker 侧 | **远程（需同步）** |
| **Bash 执行** | **本机** | **沙箱本机** | **SSH → 远程** | **远程** |
| **读/写代码** | **本机** | **沙箱本机** | **SSH/SCP → 远程** | **远程** |
| **权限检查** | Worker 侧 SecuredRuntime | Worker 侧 SecuredRuntime | Worker 侧 SecuredRuntime→远程 | **远程 Worker 侧** |
| **配置文件** | Worker 侧 `config.json` | Worker 侧 | Worker 侧 | Manager 侧下发 |
| **API Keys** | Worker 侧 `auth.json` | Worker 侧 | Worker 侧 | **远程 Worker 侧** |
| **Manager 进程** | 本机 | 本机 | 本机 | 本机（只做管理） |

---

## 每个命令可以选择不同 Runtime

**关键：不是 session 级别选择，是命令级别选择。**

```rust
// 同一个 Agent 会话中，不同的工具走不同的 Runtime
match tool_name {
    "read" | "write" | "edit" => {
        // 操作本地项目代码 → LocalRuntime
        local_rt
    }
    "bash" if is_safe(command) => {
        // 安全命令 → LocalRuntime
        local_rt
    }
    "bash" if is_risky(command) => {
        // 危险命令 → SandboxRuntime（即使在其他都不走沙箱时）
        sandbox_rt
    }
    "deploy" | "kubectl" => {
        // 部署命令 → RemoteRuntime（SSH 到生产环境）
        remote_rt
    }
    "review" | "diff" => {
        // 代码审查 → 全走远程沙箱
        SecuredRuntime::new(sandbox_rt)
    }
}
```

**实现方式：** `SecuredRuntime` 包一层选择器 Runtime，根据命令/工具名动态路由：

```rust
pub struct SelectorRuntime {
    local: LocalRuntime,
    sandbox: SandboxRuntime,
    remote: RemoteRuntime,
}

impl Runtime for SelectorRuntime {
    async fn execute_command(&self, command: &str, timeout: u64) -> ... {
        if command.starts_with("kubectl") || command.starts_with("ssh") {
            self.remote.execute_command(command, timeout).await  // 远程执行
        } else if is_risky(command) {
            self.sandbox.execute_command(command, timeout).await  // 沙箱执行
        } else {
            self.local.execute_command(command, timeout).await    // 本地执行
        }
    }
}
```

---

## 实际组合示例

### 示例 1：日常开发（全本地）

```
LLM → 本地      bash → 本地         代码 → 本地
Extension → 本地  权限 → 本地 SecuredRuntime
```

### 示例 2：命令远程，其余本地

```
LLM → 本地      bash → SSH→服务器    代码 → SSH→服务器
Extension → 本地  权限 → 本地（检查完再转发）
```

### 示例 3：敏感命令沙箱，普通命令本地

```
npm install → SandboxRuntime（沙箱隔离）
echo hello  → LocalRuntime（直接执行）
kubectl apply → RemoteRuntime（远程执行）
```

### 示例 4：全远程 Worker

```
Manager（本机）──管理──→ Worker（远程服务器）
                           LLM → 远程 API
                           bash → 远程
                           Extension → 远程 wasmtime
                           权限 → 远程 SecuredRuntime
```

---

## 当前实现状态

| Runtime 实现 | 状态 | 说明 |
|-------------|------|------|
| `LocalRuntime` | ✅ 完成 | 本地直接执行 |
| `SecuredRuntime` | ✅ 完成 | 中间件（权限+守卫+审计） |
| `SelectorRuntime` | 🔧 设计 | 按命令路由到不同 Runtime |
| `SandboxRuntime` | 🔧 设计 | macOS sandbox-exec |
| `RemoteRuntime` | 🔧 设计 | SSH/HTTP/gRPC 远程执行 |
| `WorkerRuntime` | ✅ 完成 | Worker 编排（spawn/send/kill） |
