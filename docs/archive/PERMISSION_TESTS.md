# ION 权限系统 CLI 测试规格

> **状态：测试规格** — 覆盖 9 个 Group，共 36 项测试。

---

## Group A：配置加载与合并

### A1 全局配置加载

```bash
# 准备：写全局配置
cat > ~/.ion/settings.json << 'EOF'
{
  "default_permissions": "test-profile",
  "permissions": {
    "test-profile": {
      "rules": [
        {"subject": "command.run", "pattern": "echo ok", "behavior": "allow"}
      ]
    }
  }
}
EOF

# 验证：启动 Manager → 创建 Worker → echo 应放行
ion rpc --method call_tool --params '{"tool":"bash","args":{"command":"echo ok"}}'
# 预期：success=true, output="ok\n"
```

### A2 项目配置叠加

```bash
# 准备：项目级配置（叠加规则）
cat > <project>/.ion/settings.json << 'EOF'
{
  "permissions": {
    "test-profile": {
      "rules": [
        {"subject": "command.run", "pattern": "rm *", "behavior": "deny"}
      ]
    }
  }
}
EOF

# 验证：全局 allow echo ok 仍生效，项目 deny rm -rf 也生效
ion rpc --method call_tool --params '{"tool":"bash","args":{"command":"echo ok"}}'
# 预期：success=true

ion rpc --method call_tool --params '{"tool":"bash","args":{"command":"rm -rf /tmp/x"}}'
# 预期：success=false, error 包含 "deny"
```

### A3 高优先级覆盖低优先级

```bash
# 全局规则 allow echo deny-ok
# 项目规则 deny echo deny-ok（应覆盖全局 allow）
cat > <project>/.ion/settings.json << 'EOF'
{
  "permissions": {
    "test-profile": {
      "rules": [
        {"subject": "command.run", "pattern": "echo deny-ok", "behavior": "deny"}
      ]
    }
  }
}
EOF

ion rpc --method call_tool --params '{"tool":"bash","args":{"command":"echo deny-ok"}}'
# 预期：success=false（项目 deny 覆盖全局 allow）
```

---

## Group B：规则匹配

### B1 精确匹配 allow

```bash
# 规则：allow "echo hello"
# 命令 echo hello → 放行
# 命令 echo world → 未匹配 → 走默认行为（ask 或 deny 取决于模式）
```

### B2 通配符匹配 deny

```bash
# 规则：deny "rm -rf *"
# 命令 rm -rf /tmp → 拒绝
# 命令 rm -rf / → 拒绝
# 命令 rm file.txt → 不匹配（没有 -rf）
```

### B3 前缀匹配

```bash
# 规则：allow "npm *"
# 命令 npm install → 放行
# 命令 npx create-app → 不匹配（npx ≠ npm）
```

### B4 Subject 匹配

```bash
# 规则：deny subject=file.read pattern="**/.env*"
# 工具 read 读 .env → 拒绝
# 工具 bash 执行命令 → 放行（subject 不同）
```

### B5 多规则优先级（deny > allow）

```bash
# 规则1: allow "echo *"
# 规则2: deny "echo secret*"
# 命令 echo secret-data → deny 优先
```

---

## Group C：Permission Mode

### C1 default 模式

```bash
# default_permissions = ":workspace"
# 写操作在工作区根 → 放行
# 写操作在 ~/.ssh → 拒绝
```

### C2 readonly 模式

```bash
# --permission-mode readonly
# 读文件 → 放行
# 写文件 → 拒绝
```

### C3 bypassPermissions 模式

```bash
# --permission-mode bypassPermissions
# 所有操作放行（包括高危命令）
# 但安全检查（.ssh/.env）仍拦截
```

### C4 dontAsk 模式

```bash
# --permission-mode dontAsk
# allow 规则匹配 → 放行
# 未匹配规则 → 拒绝（不询问）
```

### C5 plan 模式

```bash
# --permission-mode plan
# 读操作 → 放行
# 写操作 → 询问（ask）
```

---

## Group D：文件系统访问控制

### D1 工作区根可写

```bash
# profile = :workspace
# 在工作区根创建文件 → 放行
ion rpc --method call_tool --params '{"tool":"write","args":{"file_path":"<project>/test.txt","content":"test"}}'
# 预期：success=true
```

### D2 .env 文件禁止读取

```bash
# profile = :workspace + "**/*.env" = deny
# 读 .env 文件 → 拒绝
ion rpc --method call_tool --params '{"tool":"read","args":{"file_path":"<project>/.env"}}'
# 预期：success=false
```

### D3 .git 目录只读

```bash
# profile = :workspace（内置规则 .git = read）
# 写 .git/config → 拒绝
ion rpc --method call_tool --params '{"tool":"write","args":{"file_path":"<project>/.git/config","content":"hack"}}'
# 预期：success=false
```

### D4 SSH 目录禁止读取

```bash
# profile 规则 "~/.ssh" = deny
# 读 ~/.ssh/id_rsa → 拒绝
```

### D5 临时目录可写

```bash
# profile 规则 ":tmpdir": "write"
# 写 /tmp/test → 放行
```

---

## Group E：网络访问控制（预留）

### E1 域名 allow

```bash
# 规则：allow "api.openai.com"
# curl api.openai.com → 放行
```

### E2 域名 deny

```bash
# 规则：deny "*"
# curl tracking.example.com → 拒绝
```

### E3 通配符域名

```bash
# 规则：allow "*.github.com"
# curl api.github.com → 放行
# curl google.com → 拒绝
```

---

## Group F：作用域（Scope）

### F1 会话级规则（session）

```bash
# 通过 extension_rpc add_rule scope=session
# 当前会话生效
# 重启 Worker 后规则消失
```

### F2 项目级规则（project）

```bash
# 通过 extension_rpc add_rule scope=project
# 持久化到 settings.json
# 重启 Worker 后规则仍在
```

### F3 全局级规则（userSettings）

```bash
# 直接在 ~/.ion/settings.json 配置
# 对所有项目生效
```

---

## Group G：Profile 继承

### G1 继承内置 profile

```jsonc
{
  "permissions": {
    "my-profile": {
      "extends": ":workspace",
      "rules": [
        {"subject": "file.read", "pattern": "**/*.env", "behavior": "deny"}
      ]
    }
  }
}
```

### G2 继承自定义 profile

```jsonc
{
  "permissions": {
    "base-profile": {
      "filesystem": { ":workspace_roots": { ".": "write" } }
    },
    "extended-profile": {
      "extends": "base-profile",
      "filesystem": { ":workspace_roots": { "**/*.env": "deny" } }
    }
  }
}
```

### G3 拒绝循环继承

```jsonc
// profile A extends B, B extends A → 拒绝，报错
```

---

## Group H：边界情况

### H1 不存在的 profile

```jsonc
{ "default_permissions": "nonexistent" }
// 应回退到 :workspace
```

### H2 空规则

```jsonc
{ "permissions": { "empty": {} } }
// 无规则，所有操作走默认行为
```

### H3 超大规则列表

```jsonc
// 1000+ 条规则，性能测试
// 匹配耗时 < 100ms
```

### H4 Unicode 路径

```jsonc
// 规则 "**/中文路径/*" = deny
// 读 /tmp/中文路径/文件.txt → 拒绝
```

### H5 符号链接

```jsonc
// 规则 deny "/etc/passwd"
// 读符号链接 /tmp/link → /etc/passwd → 拒绝（追踪真实路径）
```

---

## Group I：集成场景

### I1 完整审批流程

```bash
# Terminal 1: ion subscribe --ui
# Terminal 2: 触发 ask 规则
# Terminal 1: ion rpc --method ui_respond
# Terminal 2: 命令继续执行
```

### I2 Extension 动态添加规则

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo *","behavior":"allow","scope":"session"}}'
# 然后 echo hello → 放行
```

### I3 多 Worker 共享配置

```bash
# Worker A 添加 project 级规则
# Worker B 同一项目 → 规则生效
```

### I4 禁用 Extension 回退到核心

```jsonc
// config.json: extensions.permission.enabled = false
// PermissionExtension 不加载
// 权限回退到 SecuredRuntime（CommandGuard + PermissionEngine）
```

### I5 全量验证脚本

```bash
# bash tests/permission_ci.sh
# 运行全部 A-I Group 测试
# 退出码：0=全部通过
```

---

## 测试数据准备

```bash
# 创建测试用 settings.json
export ION_CONFIG_DIR=/tmp/ion-test-config
mkdir -p $ION_CONFIG_DIR
cat > $ION_CONFIG_DIR/settings.json << 'EOF'
{
  "default_permissions": "test",
  "permissions": {
    "test": {
      "rules": [
        {"subject": "command.run", "pattern": "echo ok", "behavior": "allow"},
        {"subject": "command.run", "pattern": "rm -rf *", "behavior": "deny"},
        {"subject": "file.read", "pattern": "**/.env*", "behavior": "deny"}
      ]
    }
  }
}
EOF
```
