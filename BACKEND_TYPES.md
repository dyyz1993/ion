# Backend 类型分类

> **状态：已完成** — BackendRegistry 支持的所有 backend 类型及其使用场景。

## 关键概念：权限过滤 ≠ 环境隔离

ION 的四种 backend 按隔离强度分两大类，**这一点必须先理解清楚，否则会选错**：

| 类别 | 代表 | 隔离强度 | `rm -rf /tmp/x` 的后果 |
|------|------|---------|----------------------|
| **直接执行 / 权限过滤** | `local`, `sandbox` | 无 / 弱 | 本机 `/tmp/x` 真的被删 |
| **环境隔离** | `remote`, `container` | 强 | 只删远程/容器内的 `/tmp/x`，本机零影响 |

**判断口诀：**
- 想要"删了不影响本机" → 必须用 `remote` 或 `container`
- 想要"限制危险命令" → `sandbox` + `command_guard` 就够
- 想要"完全不管，最快速度" → `local`（信任代码时）

## 防御层级

```
┌──────────────────────────────────────────────────────────┐
│  防御层 1：CommandGuard（命令字符串过滤，所有模式都有）    │
│    - whitelist 模式：只放行白名单命令，未知命令询问用户    │
│    - 高危模式（rm -rf /、fork 炸弹）直接拒绝              │
│    - 中危模式（sudo、| sh）询问用户                       │
│    适合：拦截明显的危险命令                               │
│    局限：base64/eval/子 shell 可绕过字符串匹配            │
├──────────────────────────────────────────────────────────┤
│  防御层 2：Backend 路由（应用层，read/write 工具调用）     │
│    - read/write/edit 工具的路径走 resolve_path 精准路由   │
│    - 不在白名单路径的访问被拒绝或重定向                   │
│    适合：保护特定敏感路径（~/.ion、~/.ssh）               │
│    局限：bash 命令字符串里的路径不解析（无法拦截）        │
├──────────────────────────────────────────────────────────┤
│  防御层 3：Backend 本身（环境隔离，最强）                  │
│    - sandbox：权限过滤（共享 fs，rm 影响本机）            │
│    - container/remote：环境隔离（独立 fs，rm 不影响本机） │
│    适合：兜底防御，即使前两层被绕过也安全                 │
└──────────────────────────────────────────────────────────┘
```

**推荐配置策略：**
- 99% 日常开发：`local` + `CommandGuard.whitelist` 模式（半信任）
- 不信任代码：`container`（真隔离，bash 乱跑也跑不出容器）
- worktree 并行：`container`（每个 worktree 一个容器，同端口不同 IP）

## 核心分类

所有 backend 按执行环境特性分为 **4 大类**：

```
┌─────────────────────────────────────────────────────────┐
│                    Backend 类型                          │
├──────────────┬──────────────┬──────────────┬───────────┤
│ 直接执行     │ 连接远程     │ 权限包裹     │ 完全隔离   │
│ Direct       │ Remote       │ Wrapper      │ Container │
├──────────────┼──────────────┼──────────────┼───────────┤
│ local        │ remote       │ sandbox      │ container │
│              │              │              │ ├ apple   │
│              │              │              │ ├ docker  │
│              │              │              │ └ podman  │
└──────────────┴──────────────┴──────────────┴───────────┘
```

| 类别 | 隔离程度 | 谁创建环境 | 网络独立性 | 多实例并行 |
|------|---------|-----------|-----------|-----------|
| **Direct (local)** | 无隔离 | — | 共享主机 | ❌ 单一 |
| **Remote** | 物理隔离 | 已存在（外部） | 独立 | 受主机数限制 |
| **Wrapper (sandbox)** | 权限隔离 | — | 共享主机 | ❌ 单一 |
| **Container** | 完全隔离 | **我们创建** | **独立** | ✅ 无限 |

---

## 1. Direct — 直接执行

### `local`

直接在本机执行命令、读写本机文件系统。

```json
"local": {"type": "local"}
```

| 属性 | 值 |
|------|-----|
| 隔离 | **无**（直接执行） |
| 文件系统 | 主机文件系统 |
| `/tmp` | 主机 `/tmp` |
| 二进制来源 | 主机已装的（node/go/python 直接可用） |
| 网络 | 主机网络 |
| 生命周期 | — |
| 安全依赖 | **依赖 CommandGuard 拦截危险命令** |
| 适用场景 | 开发者本地调试、读 Skill 文件、运行 npm/cargo |

**何时用：** 信任环境、需要直接访问本机资源（如读 `~/.ion/agent/skills/*.md`）。配合 `CommandGuard.whitelist` 模式就是"半信任"——白名单命令直接跑，未知命令问用户，高危命令拒绝。

---

## 2. Remote — 连接远程

### `remote`

通过 SSH 连接到一个**已存在**的远程主机执行命令。主机不是我们创建的，只是借用其环境。

```json
"sh-sandbox": {"type": "remote", "hostname": "shanbox"}
"jd":         {"type": "remote", "hostname": "jd.internal", "user": "deploy", "port": 2200, "key": "~/.ssh/jd_key"}
"prod-jump": {"type": "remote", "hostname": "10.0.0.5", "proxy_jump": "bastion.example.com"}
```

| 属性 | 值 |
|------|-----|
| 隔离 | **真隔离**（独立机器，独立内核） |
| 文件系统 | 远程主机文件系统 |
| `/tmp` | 远程主机的 `/tmp`（不影响本机） |
| 网络 | 远程主机网络 |
| 生命周期 | 外部管理（不归我们） |
| 多实例 | 受限于已配置的主机数 |
| 适用场景 | 远程开发、远程编译、跳板机访问内网 |

**关键字段：**

| 字段 | 说明 | 默认 |
|------|------|------|
| `hostname` | 主机名或 IP（必填） | — |
| `user` | SSH 用户 | 走 ~/.ssh/config |
| `port` | SSH 端口 | 走 ~/.ssh/config |
| `key` | SSH 私钥路径 | 默认 agent |
| `proxy_jump` | 跳板机 (`-J` 参数) | 无 |

**何时用：** 远程主机已存在、不想自己管容器、需要访问远程私有网络（内网 Git、内网镜像）。

---

## 3. Wrapper — 权限过滤（**不是环境隔离**）

### `sandbox`

本机执行命令，用 macOS `sandbox-exec` 包裹，在 syscall 层加权限过滤规则。

> ⚠️ **重要：sandbox-exec 是权限过滤器，不是环境隔离器。**
>
> 它跟本机**共享同一个文件系统、同一个进程表、同一个网络栈**。它只能拦截未授权的 syscall（比如禁止写 `/etc`），不能提供独立的 `/tmp` 或独立的进程表。
>
> 对比：
> - `rm -rf /tmp/foo` 在 sandbox 里执行 → **本机的 `/tmp/foo` 真的被删**（如果 profile 允许写 /tmp）
> - `rm -rf /tmp/foo` 在 container 里执行 → **只删容器内的 `/tmp/foo`**，本机零影响
>
> **想要真隔离（rm 不影响本机），请用 [Container](#4-container--完全隔离-)，不要用 sandbox。**

```json
"sandbox-readonly":  {"type": "sandbox", "profile": "readonly"}
"sandbox-workspace": {"type": "sandbox", "profile": "workspace"}
```

| 属性 | 值 |
|------|-----|
| 隔离类型 | **权限过滤**（非环境隔离） |
| 文件系统 | **本机文件系统**（profile 控制哪些路径可读写） |
| `/tmp` | **本机 /tmp**（不是独立的） |
| 进程表 | **本机进程表** |
| 网络 | 本机网络（profile 可禁） |
| 用户 | 本机用户 |
| 二进制来源 | **本机已装的二进制**（node/go/python 直接可用） |
| 多实例 | ❌ 单一（共享主机内核） |
| 适用场景 | **信任代码 + 限制危险操作**（不要用于跑未知代码） |

**内置 profile：**

| profile | 效果 |
|---------|------|
| `readonly` | 全局只读 + /tmp 可写 + 禁止网络（**最安全的 profile**） |
| `workspace` | 工作区可写 + /tmp 可写 + 系统路径只读 + 网络允许 |
| `full-access` | 全部允许（等同无沙箱） |

> ⚠️ **已知限制：** 当前 `workspace` profile 用的是 `(allow default)` 黑名单思路（默认全开 + 选择性 deny），意味着没列到的路径（如 `/var`、`/opt`、用户家目录其他子目录）默认可写。**这是待修复的安全 bug**，计划改成白名单（默认全拒 + 选择性 allow）。在此之前，对不信任代码请用 `readonly` profile 或 Container。

**何时用 sandbox：**
- ✅ 限制信任代码的危险操作（防止 LLM 误删 `/etc`）
- ✅ macOS 主机、想用本机的 node/go/python 跑代码
- ❌ **不要**用来跑不信任的二进制或恶意代码（用 Container）
- ❌ **不要**指望 `/tmp` 跟本机隔离（它就是本机的 `/tmp`）

**何时改用 Container：**
- 想要 `rm -rf /` 不影响本机
- 想要独立的 `/tmp`、独立的进程表、独立的网络
- 想要并行多个同端口实例
- 跑不信任的代码

## 4. Container — 完全隔离 ⭐

### `container` + `driver: "apple"`

通过 Apple Container (`/usr/local/bin/container`) 创建一个独立的 Linux VM。**完全隔离**：独立文件系统、独立网络命名空间、独立 IP。

```json
"apple-dev": {
  "type": "container",
  "driver": "apple",
  "image": "docker.io/library/node:22-alpine",
  "port": 3000,
  "memory": "2G",
  "cpus": 2,
  "volume": "shared-npm-cache"
}
"apple-build": {
  "type": "container",
  "driver": "apple",
  "image": "docker.io/library/rust:1.81",
  "port": 8080
}
```

| 属性 | 值 |
|------|-----|
| 隔离 | **真隔离**（独立 VM，独立内核） |
| 文件系统 | 容器内独立文件系统（可选 mount worktree 进去） |
| `/tmp` | **容器自己的 `/tmp`**（rm 不影响本机） |
| 网络 | **独立 IP**（如 192.168.64.x） |
| 二进制来源 | **镜像自带**（本机的 node/go 不能共享，因为 Mach-O ≠ ELF） |
| 生命周期 | **我们管理**（启动/停止/清理） |
| 多实例 | ✅ **无限并行**（同端口不同 IP） |
| 适用场景 | 并行开发、同端口多环境、跑不信任代码、worktree 隔离 |

**关键字段：**

| 字段 | 说明 | 默认 |
|------|------|------|
| `driver` | 容器驱动 | `"apple"` |
| `image` | OCI 镜像（必填） | — |
| `port` | 暴露端口 | 0 (不暴露) |
| `memory` | 内存限制（如 "2G"） | 无限制 |
| `cpus` | CPU 核心数 | 无限制 |
| `volume` | 共享卷（依赖缓存） | 无 |
| `mount_path` | worktree 挂载点 | `/workspace` |

### `container` + `driver: "docker"` (未来)

通过 Docker CLI 创建标准容器。跟 Apple Container 行为一致，区别仅在底层驱动。

```json
"docker-dev": {
  "type": "container",
  "driver": "docker",
  "image": "node:22-alpine",
  "port": 3000
}
```

### `container` + `driver: "podman"` (未来)

通过 Podman（无 daemon、rootless）创建容器。

```json
"podman-build": {
  "type": "container",
  "driver": "podman",
  "image": "rust:1.81"
}
```

### Container 类的共同特点

所有 `container` 类共享这些特性，区别只在 driver：

1. **本机延伸**：从主机创建，不依赖外部已存在主机
2. **完全隔离**：独立 OS、文件系统、网络命名空间
3. **生命周期受控**：启动、停止、清理由 BackendRegistry 管理
4. **多实例并行**：可以同时跑 N 个，各自独立 IP
5. **同端口不冲突**：A、B、C 容器都用 `:3000`，因为 IP 不同

**对比 Remote：**

| 维度 | Remote | Container |
|------|--------|-----------|
| 环境来源 | 外部已存在 | **我们创建** |
| 配置 | hostname + key | image + port + memory |
| 多实例 | 受限于主机数 | **无限** |
| 网络 | 远程主机网络 | 独立 IP |
| 清理 | 不归我们 | **我们清理** |
| 适用 | 借用现有服务器 | 临时隔离环境 |

---

## 配置示例：典型场景

### 场景 0：日常开发（推荐 — 99% 场景）

**完全放开模式**（最简单，信任代码）：
```json
{
  "default": "local",
  "backends": {
    "local": {"type": "local"}
  }
}
```
默认 `CommandGuard.whitelist` 模式生效：白名单命令（npm/cargo/git）直接跑，未知命令问用户，高危命令拒绝。

**半信任模式**（LLM 生成代码、担心误操作）：
```json
{
  "default": "local",
  "backends": {
    "local": {"type": "local"}
  },
  "command_guard": {
    "mode": "whitelist",
    "whitelist": ["npm", "cargo", "git", "node", "go", "python3"]
  }
}
```
不在白名单的命令一律问用户。

### 场景 1：本地开发 + 远程编译

```json
{
  "default": "local",
  "backends": {
    "local":   {"type": "local"},
    "shanbox": {"type": "remote", "hostname": "shanbox"}
  },
  "routes": [
    {"command": "rustc *",  "target": "shanbox"},
    {"command": "cargo *",  "target": "shanbox"}
  ]
}
```

### 场景 2：远程默认 + 本地 Skill 访问

```json
{
  "default": "shanbox",
  "backends": {
    "local":   {"type": "local"},
    "shanbox": {"type": "remote", "hostname": "shanbox"}
  },
  "routes": [
    {"command": "npm *",   "target": "local"},
    {"command": "cargo *", "target": "local"},
    {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
  ]
}
```

### 场景 3：Apple Container 并行开发（同端口多实例）

```json
{
  "default": "apple-dev",
  "backends": {
    "local":       {"type": "local"},
    "apple-dev":   {"type": "container", "driver": "apple", "image": "node:22-alpine", "port": 3000},
    "apple-build": {"type": "container", "driver": "apple", "image": "rust:1.81",       "port": 8080},
    "apple-test":  {"type": "container", "driver": "apple", "image": "node:22-alpine", "port": 3000, "memory": "1G"}
  },
  "routes": [
    {"command": "cargo *",  "target": "apple-build"},
    {"command": "npm test", "target": "apple-test"},
    {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
  ]
}
```

> 注意：`apple-dev` 和 `apple-test` 都用 `port: 3000`，因为容器 IP 不同所以不冲突。

### 场景 4：混合（远程 + 沙箱 + 容器）

```json
{
  "default": "local",
  "backends": {
    "local":            {"type": "local"},
    "shanbox":          {"type": "remote",  "hostname": "shanbox"},
    "sandbox-workspace":{"type": "sandbox", "profile": "workspace"},
    "untrusted-run":    {"type": "container", "driver": "apple", "image": "alpine:latest", "memory": "512M", "cpus": 1}
  },
  "routes": [
    {"command": "kubectl *",   "target": "shanbox"},
    {"command": "curl *",      "target": "untrusted-run"},
    {"path": "/untrusted/*",   "target": "untrusted-run"}
  ]
}
```

### 场景 5：高安全模式（跑不信任代码）

**所有 bash 命令在容器里执行**，本机文件零影响。即使 base64/eval 绕过 CommandGuard，破坏的也只是容器。

```json
{
  "default": "apple-secure",
  "backends": {
    "local": {"type": "local"},
    "apple-secure": {
      "type": "container",
      "driver": "apple",
      "image": "ubuntu:24.04",
      "workspace": "/Users/xuyingzhou/Project/myapp",
      "mount_path": "/workspace",
      "memory": "2G",
      "cpus": 2
    }
  },
  "routes": [
    {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
  ]
}
```

效果：
- `bash "rm -rf /"` → 容器被破坏，本机零影响 ✅
- `bash "echo xxx | base64 -d > /tmp/evil"` → 写到容器的 /tmp，本机零影响 ✅
- `read /Users/.../.ion/skill.md` → 路由到本地，能读到 ✅
- `read /workspace/file.txt` → 容器内读（workspace 挂载） ✅

---

## Driver 扩展指南

加新 container driver（如未来加 `firecracker`、`gVisor`）只需：

1. 在 `backend_registry.rs` 的 `build_backend` match 分支加：
   ```rust
   "firecracker" => {
       let rt = FirecrackerRuntime::new(...);
       Ok(Box::new(SecuredRuntime::new(rt).with_profile(...)))
   }
   ```
2. 实现 `Runtime` trait（参考 `AppleContainerRuntime`）
3. 在本文档加一节说明

**不需要**改 BackendRegistry 路由逻辑、配置解析、Runtime trait——这些都是 driver 无关的。

---

## 参考

- 测试用例: [ROUTER_TEST_SPEC.md](./ROUTER_TEST_SPEC.md)
- Apple Container Skill: `~/.agents/skills/apple-container-worktree-sandbox/SKILL.md`
- 实现: `src/backend_registry.rs`
