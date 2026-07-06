# ION 权限系统设计

> **状态：设计稿** — 对齐 pi (pi-coding-agent)、Codex Permissions、Claude 权限三层参考。

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
