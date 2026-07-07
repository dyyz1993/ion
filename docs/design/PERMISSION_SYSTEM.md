# ION 权限系统设计

> **状态：设计稿 + 已实现（CLI 用法已验证）+ 测试规格**
>
> 本文档合并自 4 个源文档：
> - 主设计稿（§一–§八）：原 `PERMISSION_SYSTEM.md`
> - CLI 用法（§九）：原 `PERMISSION_CLI.md`（已验证）
> - CLI 测试规格（§十）：原 `PERMISSION_TESTS.md`
> - CLI 测试指南（§十一）：原 `SECURITY_CLI_GUIDE.md`（仅权限相关，Runtime 进程管理部分见 [`BASH_EXTENSION.md`](./BASH_EXTENSION.md)）

---

## 一、架构总览

```
配置层（config.json, 8 层来源）
  │
  ├── PermissionProfile 1（:read-only）
  ├── PermissionProfile 2（:workspace）
  ├── PermissionProfile 3（develop）← default_permissions
  └── PermissionProfile N（自定义）
        │
        ▼
  决策引擎（10 步流水线）
        │
        ├── 规则匹配（allow / deny / ask）
        ├── 模式检查（default / bypass / readonly / yolo）
        └── 默认行为
              │
              ▼
        ┌────┴────┐
        │ Allow   │ → 放行给 SecuredRuntime
        │ Deny    │ → 拒绝
        │ Ask     │ → UI 通道 → subscribe --ui → ui_respond
        └─────────┘
```

## 二、配置来源（8 层）

按优先级从高到低：

| 优先级 | 来源 | 路径 / 来源 | 说明 |
|--------|------|-------------|------|
| 🔺 1 | 策略配置 | `managed-settings.json` | 企业托管，不可覆盖 |
| 2 | Flag | `--permission-profile` CLI 参数 | 运行时指定 |
| 3 | 本地项目 | `<project>/.ion/settings.local.json` | gitignored |
| 4 | 项目共享 | `<project>/.ion/settings.json` | 可提交 git |
| 5 | 全局用户 | `~/.ion/settings.json` | 用户级 |
| 6 | CLI 参数 | `--allowed-tools` 等 | 运行时 |
| 7 | 命令注入 | extension_rpc 动态添加 | 会话 |
| 🔻 8 | 会话内存 | 当前 session 内存 | 临时 |

**配置合并规则**：高优先级覆盖低优先级的同名 key。数组合并（不覆盖）。

## 三、Permission Profile

### 内置 Profile

| 名称 | 效果 |
|------|------|
| `:read-only` | 所有文件系统只读，命令可执行，无网络 |
| `:workspace` | 工作区根可写，`.codex/` `.git/` 等只读，`:minimal` 可读 |
| `:danger-full-access` | 不限制，等同无沙箱 |

### 自定义 Profile

```jsonc
{
  "default_permissions": "develop",

  "permissions": {
    "develop": {
      "description": "日常开发",
      "extends": ":workspace",

      "filesystem": {
        // 特殊路径占位符
        ":minimal": "read",                     // 系统工具路径
        ":workspace_roots": {                   // 工作区根（当前 session + profile 定义）
          ".": "write",
          ".devcontainer": "read",
          "**/*.env": "deny"
        },
        ":tmpdir": "write",                     // $TMPDIR
        ":slash_tmp": "write",                  // /tmp
        "~/.ssh": "deny",
        "~/.aws": "deny"
      },

      "network": {
        "enabled": true,
        "domains": {
          "api.openai.com": "allow",
          "*.github.com": "allow",
          "*": "deny"
        }
      },

      "rules": [                                // 简单规则（兼容模式）
        {"subject": "command.run", "pattern": "npm *", "behavior": "allow"},
        {"subject": "command.run", "pattern": "rm -rf *", "behavior": "deny"},
        {"subject": "file.read", "pattern": "**/*.env", "behavior": "deny"}
      ]
    }
  }
}
```

### Profile 继承

```jsonc
{
  "permissions": {
    "my-profile": {
      "extends": ":workspace",                  // 继承内置或自定义
      "filesystem": {
        ":workspace_roots": {
          "**/*.env": "deny"                    // 仅添加差异规则
        }
      }
    }
  }
}
```

### 配置文件示例（分层）

**`~/.ion/settings.json`**（全局用户）:
```jsonc
{
  "default_permissions": "develop",
  "permissions": {
    "develop": {
      "filesystem": {
        ":workspace_roots": {
          ".": "write"
        }
      },
      "network": {
        "enabled": true,
        "domains": {
          "api.openai.com": "allow"
        }
      }
    }
  }
}
```

**`<project>/.ion/settings.json`**（项目级，叠加）:
```jsonc
{
  // 继承全局的 develop profile，项目级追加规则
  "permissions": {
    "develop": {
      "filesystem": {
        ":workspace_roots": {
          "**/*.env": "deny"
        }
      }
    }
  }
}
```

## 四、Rule 语法

### Subject（对齐 pi）

| Subject | 说明 | 匹配目标 |
|---------|------|---------|
| `command.run` | Shell 命令 | `bash`, `sh` 等工具的命令参数 |
| `file.read` | 文件读取 | `read`, `grep`, `find`, `ls` 等 |
| `file.write` | 文件写入 | `write`, `edit` 等 |
| `file.delete` | 文件删除 | `remove_file` 等 |
| `network.connect` | 网络连接 | 所有出站连接 |
| `*` | 全部 | 任何操作 |

### Pattern 匹配

| 模式 | 说明 | 示例 |
|------|------|------|
| 精确匹配 | 完全相等 | `"npm install"` |
| 前缀 `*` | 通配 | `"npm *"` |
| 路径 glob | 文件路径 | `"**/*.env"` |
| 完整 glob | 任意位置 `*` | `"gh * repo"` |

### Behavior（3 种）

| Behavior | 效果 | 优先级 |
|----------|------|--------|
| `allow` | 自动放行 | 低 |
| `deny` | 自动拒绝（最高优先级） | **高** |
| `ask` | 总是询问（绕过免疫） | **中**（但 bypass 模式仍生效） |

## 五、Permission Mode（6 种）

| 模式 | 效果 | 适用场景 |
|------|------|----------|
| `default` | 未匹配规则的操作→询问 | 正常交互式开发 |
| `acceptEdits` | 文件编辑自动允许，命令仍询问 | 快速迭代 |
| `bypassPermissions` | **全部放行（高危）** | 受信环境 |
| `dontAsk` | 从不询问（未允许即拒绝） | CI/CD |
| `plan` | 所有修改需确认 | 审查阶段 |
| `readonly` | 所有写操作拒绝 | 只读审计 |

**模式选择优先级**：
1. `--dangerously-skip-permissions` CLI flag → bypassPermissions
2. `--permission-mode` CLI flag → 指定模式
3. `settings.permissions.defaultMode` → 配置默认
4. 内置默认 → `default`

## 六、10 步决策流水线

```
1a. deny 规则匹配？             → 拒绝
1b. ask 规则匹配？              → 询问
1c. 工具自身 checkPermissions() → 工具决定
1d. 工具 deny？                → 拒绝
1e. 工具需要交互？              → 询问
1f. 内容 ask 规则（绕过免疫）    → 询问
1g. 安全检查（绕过免疫）        → 询问

2a. 权限模式检查                → bypass: 放行; plan: 询问; readonly: 拒绝写
2b. allow 规则匹配？            → 放行

3. 默认处理                    → 询问
```

## 七、与现有系统的集成

```
PermissionExtension（策略层）
  ├── before_tool_call 钩子     ← 检查自有规则表
  ├── on_extension_rpc          ← CLI 管理（add_rule / list_rules）
  ├── 从 settings.json 加载规则  ← P2 实现
  └── 支持三种 scope:
      ├── session（内存，默认）
      └── project（持久化到 settings.json）

SecuredRuntime（核心层）
  ├── CommandGuard              ← 危险命令拦截
  ├── PermissionEngine          ← 路径权限匹配
  └── resolve_ask               ← UI 通道（异步 Ask）

UI 通道
  ├── subscribe --ui             ← 订阅 UI 事件
  └── ui_respond RPC            ← 回复 Ask/Confirm/Prompt
```

## 八、边界情况

| 场景 | 处理方式 |
|------|---------|
| 多配置源同名 profile | 高优先级覆盖低优先级，数组合并 |
| extends 链 | 递归展开，拒绝循环引用 |
| deny 与 allow 冲突 | deny 优先 |
| ask 与 bypass 模式 | ask 绕过 bypass（安全免疫） |
| 未匹配任何规则 | 走默认行为（通常是 ask） |
| profile 不存在 | 回退到 `:workspace` |
| 文件系统规则与命令规则重叠 | 各自独立匹配，任意 deny 即拒绝 |

---

## 九、CLI 用法

> 来源：原 `PERMISSION_CLI.md`（状态：已验证 — 所有命令均经过 E2E 测试）

### Group A：配置文件与规则加载

#### A1 全局配置

**用途：** 用户全局权限规则，对所有项目生效。

```bash
# 准备：写入全局配置
mkdir -p ~/.ion
cat > ~/.ion/settings.json << 'EOF'
{
  "permissions": {
    "rules": [
      {"subject": "command.run", "pattern": "echo *", "decision": "allow", "scope": "project"},
      {"subject": "file.read", "pattern": "**/.env*", "decision": "deny", "scope": "project"}
    ]
  }
}
EOF

# 验证：启动 Host 后规则自动加载
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"list_rules"}'
# → {"rules": [{"subject":"command.run","pattern":"echo *","decision":"allow",...},
#               {"subject":"file.read","pattern":"**/.env*","decision":"deny",...}],
#     "count": 2}
```

**✅ 实测通过**

#### A2 项目级配置（覆盖）

**用途：** 项目级配置叠加到全局之上，同名规则以项目为准。

```bash
# 准备：写入项目级配置
mkdir -p <project>/.ion
cat > <project>/.ion/settings.json << 'EOF'
{
  "permissions": {
    "rules": [
      {"subject": "command.run", "pattern": "npm *", "decision": "allow", "scope": "project"}
    ]
  }
}
EOF

# 验证：全局规则 + 项目规则合并
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"list_rules"}'
# → 应包含 3 条规则（echo * + *.env* + npm *）
```

**✅ 实测通过**

#### A3 会话级规则（动态添加）

**用途：** 运行时通过 CLI 动态添加，仅当前会话有效。

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"gh *","decision":"allow","scope":"session"}}'
# → {"message":"rule added: command.run gh * allow session","status":"ok"}
```

**✅ 实测通过**

#### A4 项目级规则持久化

**用途：** 项目级规则写入 settings.json，重启后仍在。

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.read","pattern":"**/secret/*","decision":"deny","scope":"project"}}'
# → {"message":"rule added: file.read **/secret/* deny project","status":"ok"}

# 验证持久化：查看 settings.json
cat <project>/.ion/settings.json
# → 应包含新加的规则
```

**✅ 实测通过**

### Group B：Subject 匹配

#### B1 `command.run` — bash 命令

**用途：** `subject: "command.run"` 匹配所有 bash 工具调用。

```bash
# 规则：allow all command.run
# bash_run 命令
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'
# → success=true, output="hello\n"

# bash_kill 也受 command.run 规则影响
# bash_send 同样
```

**✅ 实测通过**

#### B2 `file.read` — 文件读取

**用途：** `subject: "file.read"` 匹配 read/grep/find/ls 工具。

```bash
# 规则：deny **/.env* for file.read

# 读 .env 文件
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"<project>/.env"}}'
# → success=false, error="[Permission] '.env' denied by extension rule"

# 读普通文件（未匹配规则，放行给 SecuredRuntime）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"<project>/src/main.rs"}}'
# → success=true
```

**✅ 实测通过**

#### B3 `file.write` — 文件写入

**用途：** `subject: "file.write"` 匹配 write/edit 工具。

```bash
# 规则：deny write to /etc/ for file.write

# 写 /etc/passwd
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"/etc/passwd","content":"hack"}}'
# → success=false（PermissionExtension 或 SecuredRuntime 拒绝）
```

**✅ 实测通过**

#### B4 `*` — 全部匹配

**用途：** `subject: "*"` 匹配所有工具。

```bash
# 规则：allow all
# 所有工具调用都放行
```

**✅ 实测通过**

### Group C：规则优先级

#### C1 deny 优先

**用途：** 同一 subject+pattern 上 deny 覆盖 allow。

```bash
# 规则1: allow command.run echo *
# 规则2: deny command.run echo secret

# echo hello → allow（仅匹配规则1）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'
# → success=true

# echo secret-data → deny（规则2优先级更高）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo secret-data"}}'
# → success=false
```

**✅ 实测通过**

#### C2 会话级 > 项目级

**用途：** 会话级规则优先于项目级规则。

```bash
# 项目级规则：deny command.run echo *
# 会话级规则：allow command.run echo ok

# echo ok → 会话级 allow 优先，放行
```

**✅ 实测通过**

#### C3 项目级 > 全局级

**用途：** 项目级配置覆盖全局配置的同名规则。

```bash
# 全局规则：allow command.run echo *
# 项目规则：deny command.run echo secret

# echo secret → 项目级 deny 生效
```

**✅ 实测通过**

### Group D：Scope 作用域

#### D1 scope=session

**用途：** 当前会话有效，不持久化到磁盘。

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo session-test","decision":"allow","scope":"session"}}'

# 当前会话：echo session-test → 放行

# 重启 Worker 后：规则消失
```

**✅ 实测通过**

#### D2 scope=project

**用途：** 持久化到 `<project>/.ion/settings.json`，重启后仍存在。

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo project-test","decision":"allow","scope":"project"}}'

# 规则写入 settings.json
# 重启 Worker 后：echo project-test → 仍放行
```

**✅ 实测通过**

### Group E：权限模式

#### E1 default 模式

**用途：** 默认行为，未匹配规则的操作 Ask 用户。

```bash
# 无规则匹配时，操作会触达 SecuredRuntime
# SecuredRuntime PermissionEngine 检查
# 如果也是 Ask → 走 UI 通道
```

**✅ 实测通过**

#### E2 bypassPermissions 模式

**用途：** 全部放行（高危）。

```bash
# --permission-mode bypassPermissions
# CommandGuard 和 PermissionEngine 仍工作
# 但 PermissionExtension 的规则检查被跳过
```

**✅ 实测通过**

### Group F：集成场景

#### F1 完整权限审批流程

**用途：** subscribe --ui + ui_respond 端到端。

```bash
# Terminal 1
ion subscribe --ui
# → 等待 UI 事件

# Terminal 2
# 触发一条 Ask 规则的操作

# Terminal 1 收到：
# {"type":"ui_event","ui_type":"Ask","request_id":"req_xxx",...}

# Terminal 1 回复：
ion rpc --method ui_respond \
  --params '{"request_id":"req_xxx","response":"allow"}'

# Terminal 2 的操作继续执行
```

**🔧 设计稿**

#### F2 SecuredRuntime 拦截

**用途：** PermissionExtension 不匹配时，放行给 SecuredRuntime。

```bash
# rm -rf / 没有在 PermissionExtension 规则中
# → 放行给 SecuredRuntime
# → SecuredRuntime.CommandGuard 拦截

ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"rm -rf /"}}'
# → {"success":false,"error":"[CommandGuard] 高危命令被拦截: 高危：删除根目录"}
```

**✅ 实测通过**

#### F3 禁用 PermissionExtension

**用途：** config.json 关闭 PermissionExtension，权限回退到核心 SecuredRuntime。

```json
{
  "extensions": {
    "permission": { "enabled": false }
  }
}
```

```bash
# PermissionExtension 不加载
# 权限检查全靠 SecuredRuntime（CommandGuard + PermissionEngine）
```

**✅ 实测通过**

### Group G：验证结果

```bash
# 启动 Host + 创建 Worker
cargo run --bin ion -- manager start
cargo run --bin ion rpc --method create_worker --params '{"cwd":"/tmp"}'

# 注册规则
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo *","decision":"allow","scope":"session"}}'
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.read","pattern":"**/.env*","decision":"deny","scope":"session"}}'

# 测试 allow
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'
# → ✅ success: true

# 测试 deny
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"rm -rf /"}}'
# → ✅ success: false (CommandGuard 拦截)

# 测试 file.read deny
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/tmp/.env"}}'
# → ✅ success: false (PermissionExtension 拦截)
```

**全部已验证通过**

---

## 十、CLI 测试规格

> 来源：原 `PERMISSION_TESTS.md`（状态：测试规格 — 覆盖 9 个 Group，共 36 项测试）

### Group A：配置加载与合并

#### A1 全局配置加载

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

# 验证：启动 Host → 创建 Worker → echo 应放行
ion rpc --method call_tool --params '{"tool":"bash","args":{"command":"echo ok"}}'
# 预期：success=true, output="ok\n"
```

#### A2 项目配置叠加

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

#### A3 高优先级覆盖低优先级

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

### Group B：规则匹配

#### B1 精确匹配 allow

```bash
# 规则：allow "echo hello"
# 命令 echo hello → 放行
# 命令 echo world → 未匹配 → 走默认行为（ask 或 deny 取决于模式）
```

#### B2 通配符匹配 deny

```bash
# 规则：deny "rm -rf *"
# 命令 rm -rf /tmp → 拒绝
# 命令 rm -rf / → 拒绝
# 命令 rm file.txt → 不匹配（没有 -rf）
```

#### B3 前缀匹配

```bash
# 规则：allow "npm *"
# 命令 npm install → 放行
# 命令 npx create-app → 不匹配（npx ≠ npm）
```

#### B4 Subject 匹配

```bash
# 规则：deny subject=file.read pattern="**/.env*"
# 工具 read 读 .env → 拒绝
# 工具 bash 执行命令 → 放行（subject 不同）
```

#### B5 多规则优先级（deny > allow）

```bash
# 规则1: allow "echo *"
# 规则2: deny "echo secret*"
# 命令 echo secret-data → deny 优先
```

### Group C：Permission Mode

#### C1 default 模式

```bash
# default_permissions = ":workspace"
# 写操作在工作区根 → 放行
# 写操作在 ~/.ssh → 拒绝
```

#### C2 readonly 模式

```bash
# --permission-mode readonly
# 读文件 → 放行
# 写文件 → 拒绝
```

#### C3 bypassPermissions 模式

```bash
# --permission-mode bypassPermissions
# 所有操作放行（包括高危命令）
# 但安全检查（.ssh/.env）仍拦截
```

#### C4 dontAsk 模式

```bash
# --permission-mode dontAsk
# allow 规则匹配 → 放行
# 未匹配规则 → 拒绝（不询问）
```

#### C5 plan 模式

```bash
# --permission-mode plan
# 读操作 → 放行
# 写操作 → 询问（ask）
```

### Group D：文件系统访问控制

#### D1 工作区根可写

```bash
# profile = :workspace
# 在工作区根创建文件 → 放行
ion rpc --method call_tool --params '{"tool":"write","args":{"file_path":"<project>/test.txt","content":"test"}}'
# 预期：success=true
```

#### D2 .env 文件禁止读取

```bash
# profile = :workspace + "**/*.env" = deny
# 读 .env 文件 → 拒绝
ion rpc --method call_tool --params '{"tool":"read","args":{"file_path":"<project>/.env"}}'
# 预期：success=false
```

#### D3 .git 目录只读

```bash
# profile = :workspace（内置规则 .git = read）
# 写 .git/config → 拒绝
ion rpc --method call_tool --params '{"tool":"write","args":{"file_path":"<project>/.git/config","content":"hack"}}'
# 预期：success=false
```

#### D4 SSH 目录禁止读取

```bash
# profile 规则 "~/.ssh" = deny
# 读 ~/.ssh/id_rsa → 拒绝
```

#### D5 临时目录可写

```bash
# profile 规则 ":tmpdir": "write"
# 写 /tmp/test → 放行
```

### Group E：网络访问控制（预留）

#### E1 域名 allow

```bash
# 规则：allow "api.openai.com"
# curl api.openai.com → 放行
```

#### E2 域名 deny

```bash
# 规则：deny "*"
# curl tracking.example.com → 拒绝
```

#### E3 通配符域名

```bash
# 规则：allow "*.github.com"
# curl api.github.com → 放行
# curl google.com → 拒绝
```

### Group F：作用域（Scope）

#### F1 会话级规则（session）

```bash
# 通过 extension_rpc add_rule scope=session
# 当前会话生效
# 重启 Worker 后规则消失
```

#### F2 项目级规则（project）

```bash
# 通过 extension_rpc add_rule scope=project
# 持久化到 settings.json
# 重启 Worker 后规则仍在
```

#### F3 全局级规则（userSettings）

```bash
# 直接在 ~/.ion/settings.json 配置
# 对所有项目生效
```

### Group G：Profile 继承

#### G1 继承内置 profile

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

#### G2 继承自定义 profile

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

#### G3 拒绝循环继承

```jsonc
// profile A extends B, B extends A → 拒绝，报错
```

### Group H：边界情况

#### H1 不存在的 profile

```jsonc
{ "default_permissions": "nonexistent" }
// 应回退到 :workspace
```

#### H2 空规则

```jsonc
{ "permissions": { "empty": {} } }
// 无规则，所有操作走默认行为
```

#### H3 超大规则列表

```jsonc
// 1000+ 条规则，性能测试
// 匹配耗时 < 100ms
```

#### H4 Unicode 路径

```jsonc
// 规则 "**/中文路径/*" = deny
// 读 /tmp/中文路径/文件.txt → 拒绝
```

#### H5 符号链接

```jsonc
// 规则 deny "/etc/passwd"
// 读符号链接 /tmp/link → /etc/passwd → 拒绝（追踪真实路径）
```

### Group I：集成场景

#### I1 完整审批流程

```bash
# Terminal 1: ion subscribe --ui
# Terminal 2: 触发 ask 规则
# Terminal 1: ion rpc --method ui_respond
# Terminal 2: 命令继续执行
```

#### I2 Extension 动态添加规则

```bash
ion rpc --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo *","behavior":"allow","scope":"session"}}'
# 然后 echo hello → 放行
```

#### I3 多 Worker 共享配置

```bash
# Worker A 添加 project 级规则
# Worker B 同一项目 → 规则生效
```

#### I4 禁用 Extension 回退到核心

参见 §九 Group F3，`config.json: extensions.permission.enabled = false` 时 PermissionExtension 不加载，权限回退到 SecuredRuntime（CommandGuard + PermissionEngine）。

#### I5 全量验证脚本

```bash
# bash tests/permission_ci.sh
# 运行全部 A-I Group 测试
# 退出码：0=全部通过
```

### 测试数据准备

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

---

## 十一、CLI 测试指南

> 来源：原 `SECURITY_CLI_GUIDE.md`（状态：设计稿 — 部分 CLI 已就绪，权限审批流程为架构设计）
>
> **范围说明**：原文档涵盖 Runtime 进程管理 + UI 事件通道 + 权限 Extension 三层。本文档仅保留权限相关内容。Runtime 进程管理（前台/后台 spawn、kill、send_stdin、超时切后台）的 CLI 测试见 [`BASH_EXTENSION.md`](./BASH_EXTENSION.md)。

### 概览

ION 安全体系的 CLI 测试覆盖：

| 层 | 能力 | CLI | 状态 |
|----|------|-----|------|
| **Runtime 层** | 进程管理、文件操作全部统一走 Runtime trait，经 `SecuredRuntime` 中间件 | `ion rpc --method call_tool` | ✅ 已实现（详见 [`BASH_EXTENSION.md`](./BASH_EXTENSION.md)） |
| **UI 事件通道** | 通用人类交互推送（Ask/Confirm/Notif/Alert/Prompt） | `ion subscribe --ui` + `ion rpc --method ui_respond` | 🔧 设计稿 |
| **权限 Extension** | 在 UI 通道之上实现权限策略，支持"记住本次/本会话/全局" | 内置 Extension + 可选 WASM | 🔧 设计稿 |

### Group B：权限安全 CLI（当前可用）

#### B1 配置权限规则

权限规则通过 `PermissionEngine` 配置，当前通过代码硬编码或 `profile` 注入：

```bash
# 在 config.json 中配置安全配置文件
# ~/.ion/config.json
{
  "security": {
    "profile": "strict",     # none | strict | custom
    "rules": [
      {"name": "block-ssh", "actions": ["Read"], "pattern": "**/.ssh/*", "policy": "Deny", "priority": 100},
      {"name": "ask-tmp", "actions": ["Write"], "pattern": "/tmp/**", "policy": "Ask", "priority": 50}
    ],
    "command_guard": {
      "whitelist": ["npm", "git", "cargo", "echo", "ls", "cat"],
      "risk_patterns": [
        {"pattern": "rm -rf /", "level": "High", "message": "危险: 删除根目录"}
      ]
    }
  }
}
```

#### B2 权限拦截验证（Deny）

```bash
# 配置规则禁止读 ~/.ssh/，尝试读取应被拦截
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"path":"/Users/test/.ssh/id_rsa"}}'

# 预期：{"success":false,"error":"[Permission] 规则 'block-ssh' 拒绝了 read on ..."}
```

#### B3 命令守卫拦截（Deny）

```bash
# 高危命令被拦截
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"rm -rf /"}}'

# 预期：{"success":false,"error":"spawn rejected: ..."}
```

#### B4 安全命令放行

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo safe"}}'

# 预期：{"success":true,"data":"safe\n"}
```

### Group C：UI 事件通道 CLI（Ask/Confirm/Prompt/Notif/Alert）

#### C1 订阅 UI 事件

```bash
# Terminal 1：订阅 UI 事件通道
ion subscribe --ui

# 预期：连接保持，收到事件时逐行打印 JSON
# {"type":"ui_event","ui_type":"Ask","request_id":"req_abc123","data":{...}}
# {"type":"ui_event","ui_type":"AskResolved","data":{"response":"allow"}}
# {"type":"ui_event","ui_type":"Notif","data":{"title":"...","message":"..."}}
```

#### C2 触发权限 Ask（端到端）

```bash
# Terminal 1（先订阅）：
ion subscribe --ui

# Terminal 2（触发权限规则命中 Ask）：
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"path":"/tmp/ask-test"}}'

# Terminal 1 收到的 Ask 事件：
# {"type":"ui_event","ui_type":"Ask","request_id":"req_xxx",
#  "title":"权限请求","message":"工具想要 Read 路径: /tmp/ask-test"}

# Terminal 1 回复：
ion rpc --method ui_respond \
  --params '{"request_id":"req_xxx","response":"allow"}'

# Terminal 1 收到 AskResolved：
# {"type":"ui_event","ui_type":"AskResolved",
#  "data":{"request_id":"req_xxx","response":"allow"}}

# Terminal 2 的 call_tool 继续执行并返回结果
```

#### C3 Extension 触发 Ask（WASM）

```bash
# 一个 WASM extension 调用 host_ui_ask 时：
# WASM 端：host_ui_ask("确认删除?", "确定要删除任务 xxx 吗？")
# → 返回 0（拒绝）或 1（允许）

# 订阅者收到：
# {"type":"ui_event","ui_type":"Ask","source":"extension:todo_extension",
#  "request_id":"req_def456","title":"确认删除","message":"确定要删除任务 xxx 吗？"}
```

#### C4 通知和告警

```bash
# Notif（通知，不需要回复）
# {"type":"ui_event","ui_type":"Notif","data":{"title":"任务完成","message":"编译成功"}}

# Alert（告警，不需要回复）
# {"type":"ui_event","ui_type":"Alert","data":{"title":"磁盘不足","message":"剩余 1GB","level":"warning"}}
```

#### C5 UI 事件通道架构

```
三条独立通道：
Instance (--session x)  → agent_start/text_delta/agent_end（机器消费）
Extension (--extension) → 扩展自定义事件 + extension_rpc（调试/前端）
UI (--ui)              → Ask/Confirm/Prompt/Notif/Alert（人类交互）
                          回复走 ui_respond RPC（不走订阅通道）
```

### Group D：E2E 测试清单

> Runtime 进程管理 E2E（D1 系列）见 [`BASH_EXTENSION.md`](./BASH_EXTENSION.md)。此处仅保留权限与 UI 通道相关清单。

#### D2 权限拦截

| # | 测试 | CLI | 预期 | 状态 |
|---|------|-----|------|------|
| D2.1 | Deny 规则拦截读取 | call_tool read "~/.ssh/id_rsa" | Err [Permission] | ✅ 通过 |
| D2.2 | CommandGuard 拦截高危 | call_tool bash "rm -rf /" | Err "rejected" | ✅ 通过 |
| D2.3 | 白名单命令放行 | call_tool bash "echo safe" | Ok | ✅ 通过 |
| D2.4 | grep_search 也走权限检查 | grep_search 命中 Deny 规则 | Err | ✅ 通过 |

#### D3 UI 事件通道

| # | 测试 | 步骤 | 预期 | 状态 |
|---|------|------|------|------|
| D3.1 | subscribe_ui 过滤 | 扩展事件 vs UI 事件 | subscribe_ui 只收 UI 事件 | ✅ 通过 |
| D3.2 | subscribe 不过滤 UI | UI 事件 vs 扩展事件 | 普通 subscribe 不过滤 UI | ✅ 通过 |
| D3.3 | subscribe_all 收全部 | UI + 扩展事件 | 两条都收到 | ✅ 通过 |
| D3.4 | Ask 事件构造 | ExtensionEvent::new_ui | route="ui", custom_type="Ask" | ✅ 通过 |
| D3.5 | AskResolved 事件 | 带 response/resolved_by | data 格式正确 | ✅ 通过 |
| D3.6 | SecuredRuntime 异步 Ask | resolve_ask 三路径 | 同步/异步/拒绝 | ✅ 通过 |
| D3.7 | WASM host_ui_ask | 宿主函数注册 | 能推 Ask 事件并等回复 | ✅ 通过 |

#### D4 混合场景

| # | 测试 | 说明 | 状态 |
|---|------|------|------|
| D4.1 | 权限 + 进程管理 | 后台进程也过 CommandGuard | ✅ 通过 |
| D4.2 | PermissionExtension | before_tool_call 规则匹配 | ✅ 通过 |
| D4.3 | extension_rpc add_rule | CLI 添加规则 | ✅ 通过 |

### 验证结果（142 tests 全绿）

```
=== cargo test --lib ===
test result: ok. 98 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s

=== cargo test --test ui_event_tests ===
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0.00s

=== cargo test --test runtime_tests ===
test result: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.01s

=== cargo test --test secured_runtime_tests ===
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

=== cargo test --test extension_tests ===
test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
```

### 架构：两层设计

#### 第 1 层：核心 — 通用 UI 事件通道

核心只提供 5 种通用 UI 事件类型，**不包含任何权限专用的语义**：

| 事件 | 推送时机 | 需要回复 | 回复格式 |
|------|---------|---------|---------|
| `Ask` | 需要用户确认/拒绝 | ✅ | `"allow"` / `"deny"` |
| `Confirm` | 需要用户确认操作 | ✅ | `"confirm"` / `"cancel"` |
| `Prompt` | 需要用户输入文本 | ✅ | `"input"` + `data:"..."` |
| `Notif` | 通知/提示 | ❌ | — |
| `Alert` | 告警 | ❌ | — |

任何 Extension（包括内核的 `SecuredRuntime`、内置 Extension、WASM Extension）都可以通过这 5 种事件与人类用户交互。

#### 第 2 层：策略 — 权限 Extension

权限审批是一个**策略层问题**，不内嵌在核心中。权限 Extension 在核心之上实现：

```
Permission Extension（内置或 WASM）
├── 通过 before_tool_call 钩子拦截工具调用
├── 检查自有规则表（支持三种作用域）:
│   ├── "本次允许"（仅本次生效，不持久化）
│   ├── "本会话允许"（当前会话内有效）
│   └── "全局允许"（持久化到文件，永久生效）
├── 需要用户决策时 → 通过 UI 通道发 Ask
├── 收到 AskResolved → 存储决策到规则表
└── 下次同一规则命中 → 从规则表直接 Allow（不触发 Ask）
```

这样设计的好处：

| 能力 | 没有 Permission Extension | 有内置 Permission Extension |
|------|--------------------------|----------------------------|
| 基础 Deny/Allow 规则 | ✅ PermissionEngine 直接处理 | ✅ 一样 |
| Ask 弹出 | ✅ 通过 UI 通道 | ✅ 通过 UI 通道 |
| "记住本次选择" | ❌ | ✅ Extension 存会话级规则 |
| "本会话允许所有读" | ❌ | ✅ |
| "除非我改，永远允许这个路径" | ❌ | ✅ 持久化规则 |
| 钉钉/邮件审批 | ❌ | ✅ 写个 WASM Extension 即可 |

#### 三条独立通道

ION 有三条并行的事件通道：

```
命令                             状态         通道
────                              ─────       ────────
ion rpc --method call_tool         ✅ 已实现    RPC（工具经 Runtime）
ion subscribe --session x           ✅ 已实现    Instance 通道
ion subscribe --extension memory    ✅ 已实现    Extension 通道
ion subscribe --ui                  🔧 设计稿    UI 通道 ← 第三通道
ion rpc --method ui_respond         🔧 设计稿    UI 通道（回复）
host_ui_ask / host_ui_confirm       🔧 设计稿    WASM Extension → UI 通道
```

| 组件 | 状态 | 说明 |
|------|------|------|
| LocalRuntime 进程管理 | ✅ 已实现 | spawn_process/kill_process/send_stdin |
| SecuredRuntime 装饰器 | ✅ 已实现 | CommandGuard + PermissionEngine |
| PermissionEngine | ✅ 已实现 | Allow/Deny/Ask 三级 |
| UiSystem | ✅ 已实现 | confirm_handler 同步确认 |
| 权限事件订阅通道 | 🔧 设计稿 | EventBus 扩展 EventRoute::Permission |
| 异步 Ask 流程 | 🔧 设计稿 | SecuredRuntime 异步化 |
| `ion permission respond` CLI | 🔧 设计稿 | 回复命令 |

---

## 附录：源文档映射

| 章节 | 源文档 | 原状态 |
|------|--------|--------|
| §一 架构总览 | `PERMISSION_SYSTEM.md` | 设计稿 |
| §二 配置来源（8 层） | `PERMISSION_SYSTEM.md` | 设计稿 |
| §三 Permission Profile | `PERMISSION_SYSTEM.md` | 设计稿 |
| §四 Rule 语法 | `PERMISSION_SYSTEM.md` | 设计稿 |
| §五 Permission Mode（6 种） | `PERMISSION_SYSTEM.md` | 设计稿 |
| §六 10 步决策流水线 | `PERMISSION_SYSTEM.md` | 设计稿 |
| §七 与现有系统的集成 | `PERMISSION_SYSTEM.md` | 设计稿 |
| §八 边界情况 | `PERMISSION_SYSTEM.md` | 设计稿 |
| §九 CLI 用法 | `PERMISSION_CLI.md` | 已验证 |
| §十 CLI 测试规格 | `PERMISSION_TESTS.md` | 测试规格 |
| §十一 CLI 测试指南 | `SECURITY_CLI_GUIDE.md` | 设计稿（仅权限部分，Runtime 进程管理见 `BASH_EXTENSION.md`） |
