# ION 安全边界

## 安全不变量

1. **RemoteRuntime 不允许 fallback 到本地副作用操作。**
   - `spawn_process` / `kill_process` / `send_stdin` 返回明确 Err
   - 仅有 `execute_command` 调用 inner（用于执行 SSH 命令）
   - 已通过 `PoisonRuntime` 测试验证

2. **SandboxRuntime 只约束 `execute_command`，文件操作由 PermissionEngine 控制。**
   - bash 命令在 `sandbox-exec` 沙箱内执行
   - 文件读写透传给 inner，暴露给 `SecuredRuntime.PermissionEngine`

3. **所有工具执行必须经过 `SecuredRuntime`。**
   - CommandGuard 拦截危险命令
   - PermissionEngine 控制路径访问
   - resolve_ask 默认超时拒绝

4. **Extension 只能参与策略，不允许绕过 `SecuredRuntime`。**
   - `before_tool_call` 可自定义规则，但不能绕过安全检查

5. **Ask / approve 默认超时拒绝。**
   - `resolve_ask` 120 秒超时 → 拒绝

6. **远端 path / command 统一 escaping。**
   - 所有远端参数经 `sh_quote()` 处理
   - 防止 shell 注入
