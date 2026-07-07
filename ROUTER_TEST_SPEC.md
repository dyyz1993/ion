# RouterRuntime 测试规格

> **状态：已完成** — 测试用例文档，待实现后逐条验证。

## 架构分层

RouterRuntime 三层结构：

```
┌──────────────────────────────────────┐
│           RouterRuntime               │  ← 路由总入口
│                                       │
│  execute_command  →  命令前缀匹配     │
│  read/write/...    →  路径前缀匹配     │
│                                       │
│  ┌──── 默认 ────┐  ┌──── 例外 ────┐  │
│  │ default_mode │  │ routes[]     │  │
│  │ local/remote │  │ command/path │  │
│  │ sandbox      │  │ → target     │  │
│  └──────────────┘  └──────────────┘  │
│                                       │
│  ┌──── 后端池 ────────────────────┐  │
│  │ LocalRuntime                    │  │
│  │ RemoteRuntime(shanbox, jd, ...) │  │
│  │ SandboxRuntime(workspace)       │  │
│  └─────────────────────────────────┘  │
└──────────────────────────────────────┘
```

配置语义：

```json
{
  "runtime": {
    "backends": {
      "local":              {"type": "local"},
      "sh-sandbox":         {"type": "remote",  "hostname": "shanbox"},
      "jd":                 {"type": "remote",  "hostname": "jd.internal"},
      "sandbox-workspace":  {"type": "sandbox", "profile": "workspace"},
      "apple-dev":          {"type": "apple_container", "image": "docker.io/library/node:22-alpine", "port": 3000},
      "apple-build":        {"type": "apple_container", "image": "docker.io/library/rust:1.81", "port": 8080, "memory": "2G", "cpus": 2}
    },
    "default": "sh-sandbox",
    "routes": [
      {"command": "npm *",     "target": "local"},
      {"command": "cargo *",   "target": "local"},
      {"path": "/Users/xuyingzhou/.ion/*",     "target": "local"},
      {"path": "/Users/xuyingzhou/Project/*",  "target": "local"},
      {"path": "/workspace/build/*",           "target": "apple-build"}
    ]
  }
}
```

**三层数据模型：**

| 层 | 字段 | 含义 |
|---|------|------|
| 1. 后端定义 | `backends: {name → spec}` | 每个后端有 `type` + 类型专属字段 |
| 2. 默认行为 | `default: "name"` | 未匹配任何例外规则时走这里 |
| 3. 例外规则 | `routes: [...]` | 仅列与 default 不一致的规则 |

**支持的 `type`：**

| type | 说明 | 专属字段 |
|------|------|---------|
| `local` | 直接本地执行 | 无 |
| `remote` | SSH 到远程主机 | `hostname`, `user`, `port`, `key`, `proxy_jump` |
| `sandbox` | macOS sandbox-exec 包裹 | `profile` (workspace / readonly / full-access) |
| `apple_container` | Apple Container VM 隔离 | `image`, `port`, `memory`, `cpus`, `volume` |

**Apple Container 后端特点：**

- 每个 backend 启动一个独立的容器实例，分配独立 IP（如 `192.168.64.x`）
- 多个 apple_container backend 可同时存在（dev / build / test 各一个）
- 同一 `port` 字段在不同 backend 中可重复，因为 IP 不同
- 适合需要"同端口多环境并行开发"的场景
- 内部实现：通过 `/usr/local/bin/container` CLI 创建/管理容器
- 启动时挂载 worktree 到 `/workspace`，命令在容器内执行

**路由匹配顺序：**

1. 遍历 `routes[]`，按出现顺序匹配（命令规则只对 execute_command 生效，路径规则只对文件操作生效）
2. 第一条匹配的规则获胜（不再继续匹配）
3. 未匹配任何规则 → 走 `default`

**注意事项：**

- `default` 必须引用 `backends` 中已定义的后端名，否则回退 `local` 并打 warn
- `routes[].target` 同样必须引用已定义的后端名
- `routes[].command` 和 `routes[].path` 二者至少一个非空，否则该规则无效
- 路径规则匹配前必须做规范化（`~` 展开、`..` 解析、`//` 合并）

---

## R1 — 基础路由

### R1.1 默认 local，无例外规则

**配置：** `default: "local"`，routes 为空

| # | 操作 | 预期 |
|---|------|------|
| 1 | `execute_command("echo hello", 30)` | LocalRuntime |
| 2 | `read_file("/tmp/x.txt")` | LocalRuntime |
| 3 | `write_file("/tmp/x.txt", "hi")` | LocalRuntime |

### R1.2 默认 remote，无例外规则

**配置：** `default: "remote"`, `default_host: "shanbox"`

| # | 操作 | 预期 |
|---|------|------|
| 4 | `execute_command("echo hello", 30)` | RemoteRuntime(shanbox) |
| 5 | `read_file("/root/test.rs")` | RemoteRuntime(shanbox) |
| 6 | `write_file("/root/test.rs", "fn main() {}")` | RemoteRuntime(shanbox) |

### R1.3 默认 sandbox，无例外规则

**配置：** `default: "sandbox"`, `sandbox.profile: "workspace"`

| # | 操作 | 预期 |
|---|------|------|
| 7 | `execute_command("curl http://evil", 30)` | SandboxRuntime(workspace) — 网络被禁 |
| 8 | `read_file("/usr/share/zoneinfo/UTC")` | LocalRuntime 文件操作（沙箱只约束 bash） |

---

## R2 — 例外规则：命令前缀

### R2.1 默认 remote，npm/cargo 走 local

**配置：** `default: "remote"`, routes: `[{command: "npm *", target: "local"}, {command: "cargo *", target: "local"}]`

| # | 操作 | 预期 |
|---|------|------|
| 9 | `execute_command("npm install", 60)` | LocalRuntime ✅ |
| 10 | `execute_command("npm run build", 60)` | LocalRuntime ✅ |
| 11 | `execute_command("cargo build", 120)` | LocalRuntime ✅ |
| 12 | `execute_command("rustc main.rs", 60)` | RemoteRuntime(shanbox) — 未匹配例外 |
| 13 | `execute_command("echo hello", 30)` | RemoteRuntime(shanbox) — 未匹配例外 |

### R2.2 命令前缀精确匹配

| # | 场景 | 命令 | 预期 |
|---|------|------|------|
| 14 | 完全匹配 | `npm`（无参数） | 取决于 pattern 是否包含无参数情况 |
| 15 | 子串不应误匹配 | `npm-check-updates` | 不应匹配 `npm *` |

### R2.3 通配符语义

`npm *` 匹配：

| # | 命令 | 匹配? |
|---|------|-------|
| 16 | `npm install lodash` | ✅ |
| 17 | `npm` | ❌（pattern 要求通配符触发） |
| 18 | `npm install && cargo build` | ❌（`&&` 后不是 `npm`） |

---

## R3 — 例外规则：路径前缀

### R3.1 默认 remote，本地路径走 local

**配置：** `default: "remote"`, routes: `[{path: "/Users/xuyingzhou/.ion/*", target: "local"}]`

| # | 操作 | 预期 |
|---|------|------|
| 19 | `read_file("/Users/xuyingzhou/.ion/agent/skills/build.md")` | LocalRuntime ✅ |
| 20 | `write_file("/Users/xuyingzhou/.ion/agent/skills/custom.md", "...")` | LocalRuntime ✅ |
| 21 | `read_file("/tmp/test.rs")` | RemoteRuntime(shanbox) — 未匹配 |
| 22 | `edit_file("/Users/xuyingzhou/.ion/agent/skills/build.md", "a", "b")` | LocalRuntime ✅ |

### R3.2 路径规范化

| # | 输入路径 | 规范化后 | 预期 Runtime |
|---|----------|----------|-------------|
| 23 | `~/Project/foo/src/main.rs` | `/Users/xuyingzhou/Project/foo/src/main.rs` | 取决于 {} 配置 |
| 24 | `/Users/xuyingzhou/.ion/../.ion/skill.md` | `/Users/xuyingzhou/.ion/skill.md` | 匹配 local |
| 25 | `/Users/xuyingzhou//.ion//skill.md` | `/Users/xuyingzhou/.ion/skill.md` | 匹配 local |
| 26 | `/tmp/../../../etc/passwd` | `/etc/passwd` | 匹配 default |

### R3.3 不存在路径的处理

| # | 场景 | 操作 | 预期 |
|---|------|------|------|
| 27 | 挂载路径不存在 | `read_file("/nonexistent_mount/x.txt")` | 不匹配 → 走 default，后端返回错误 |
| 28 | 路径有空格 | `read_file("/Users/xuyingzhou/.ion/agent/skills/my skill.md")` | 匹配 local |

---

## R4 — 路由优先级

### R4.1 routes 优先于 default

| # | 场景 | 预期 |
|---|------|------|
| 29 | 匹配 routes 列表中第一条规则 | 使用该规则的 target，不走 default |
| 30 | 匹配多条规则 | 第一条匹配获胜 |

### R4.2 最长路径前缀（当有多条路径规则时）

**配置：** `routes: [{path: "/Users/xuyingzhou/*", target: "remote"}, {path: "/Users/xuyingzhou/.ion/*", target: "local"}]`

| # | 输入路径 | 匹配规则 | 预期 Runtime |
|---|----------|----------|-------------|
| 31 | `/Users/xuyingzhou/.ion/skill.md` | 第二条（最长匹配） | LocalRuntime |
| 32 | `/Users/xuyingzhou/Project/foo.md` | 第一条（仅匹配通配） | RemoteRuntime |
| 33 | `/Users/xuyingzhou/.ion/../Project/foo.md` | 规范化后匹配第一条 | RemoteRuntime |

### R4.3 命令规则优先级高于路径规则

当同一操作同时匹配命令规则和路径规则？

| # | 操作 | 两规则 | 预期 |
|---|------|--------|------|
| 34 | `execute_command("npm i")` | 命令规则: npm→local, 路径不适用 | LocalRuntime — 命令规则优先于默认 |

> **设计决策：** execute_command 只看命令前缀，不解析参数里的路径。read_file/write_file 只看路径。二者互不交叉。

---

## R5 — 安全不变量

| # | 不变量 |
|---|--------|
| 35 | 所有 read_file / write_file / edit_file / remove_file / path_exists / list_dir / find_files / grep_search / file_info **必须**经过路径路由，不可绕过 |
| 36 | 路径必须先规范化（`~` 展开、`..` 解析、`//` 合并）再匹配，原始字符串不传给后端 |
| 37 | execute_command 只看命令字符串前缀，不解析内部路径参数 |
| 38 | 路径规则不支持 `~` 写法。配置中的 `path` 必须是绝对路径 |
| 39 | 未匹配任何例外的操作走 default，不能静默失败 |

---

## R6 — 配置错误处理

| # | 场景 | 行为 |
|---|------|------|
| 40 | `default` 引用不存在的 backend 名 | 回退到 local，打印 warn |
| 41 | `routes[].target` 引用不存在的 backend 名 | 该规则无效，跳过，走 default |
| 42 | `routes[].command` 和 `routes[].path` 同时为空 | 该规则无效，跳过 |
| 43 | `routes[].command` 和 `routes[].path` 同时存在 | 两个都尝试匹配，任一匹配则命中 |
| 44 | `backends` 为空 | 全部走内置 LocalRuntime（兼容老配置） |
| 45 | backend type 未识别（如 "docker"） | 该 backend 不创建，引用时回退 default + warn |

---

## R6b — Backends 注册

### R6b.1 多 backend 共存

**配置：** backends 含 local + 2 个 remote + 1 个 sandbox + 2 个 apple_container

| # | 验证点 | 预期 |
|---|--------|------|
| 46 | `backends` HashMap 含全部 6 个条目 | ✅ |
| 47 | 每个后端类型字段正确 | local.remote=2, sandbox=1, apple_container=2 |
| 48 | 每个 RemoteRuntime 绑定正确 hostname | shanbox/jd 各自的 hostname |

### R6b.2 apple_container backend 参数

| # | 字段 | 验证 |
|---|------|------|
| 49 | `image` | 必填，否则创建失败 |
| 50 | `port` | 可选，默认 0（不暴露端口） |
| 51 | `memory` / `cpus` | 可选，传给 `container run --memory --cpus` |
| 52 | `volume` | 可选，挂载共享卷 |

### R6b.3 多 apple_container 实例同端口

**配置：** `apple-dev` (port=3000) + `apple-build` (port=3000)

| # | 操作 | 预期 |
|---|------|------|
| 53 | 创建 `apple-dev` 容器 | 分配 IP1，暴露 :3000 |
| 54 | 创建 `apple-build` 容器 | 分配 IP2（不同 IP），同样 :3000 |
| 55 | 两个容器互不干扰 | 各自独立文件系统 + 网络 |
| 56 | `execute_command` 在 apple-dev 内 | 命令在该容器内执行 |
| 57 | 同命令在 apple-build 内 | 命令在另一容器内执行 |

### R6b.4 backend 生命周期

| # | 场景 | 行为 |
|---|------|------|
| 58 | Worker 启动时 | 按需启动配置的 backend（lazy 或 eager 由实现决定） |
| 59 | Worker 关闭时 | 清理创建的容器/连接（apple_container 调 `container stop`） |
| 60 | 默认配置（无 backends 字段） | 自动注入 `local` backend，行为同当前 LocalRuntime |

---

## R7 — 集成测试（需要真实后端）

### R7.1 默认 remote + 本地路径豁免

**配置：** default=remote(shanbox), routes=[path=~/.ion→local]

| # | 步骤 | 预期 |
|---|------|------|
| 45 | Host 启动 | ✅ |
| 46 | 创建 Worker | ✅ |
| 47 | `bash "hostname"` | 返回远程主机名（shanbox） |
| 48 | `read /Users/xuyingzhou/.ion/agent/skills/build.md` | 返回本地内容 |
| 49 | `write /Users/xuyingzhou/.ion/agent/skills/test.md "hi"` | 写入本地 |
| 50 | `read /tmp/test.md` | 走远程（未匹配路径规则） |

### R7.2 命令前缀豁免

**配置：** default=remote(shanbox), routes=[command=npm→local]

| # | 步骤 | 预期 |
|---|------|------|
| 51 | `bash "npm --version"` | 返回本地 npm 版本 |
| 52 | `bash "node --version"` | 返回远程 node 版本（未匹配） |

---

## 测试数据

### 路径规范化参考

| 输入 | 规范化结果 |
|------|-----------|
| `~/.ion/skill.md` | `/Users/xuyingzhou/.ion/skill.md` |
| `/usr/local/../bin` | `/usr/bin` |
| `/usr//local///bin` | `/usr/local/bin` |
| `/a/./b/../c` | `/a/c` |
| `/tmp/../../../etc` | `/etc` |
| `~root/.ion/skill.md` | 保持 `~root` 不变（扩展 `~user` 需解析 passwd） |

---

## 验收标准

- [ ] R1 默认行为：8 条全部通过
- [ ] R2 命令前缀：10 条全部通过
- [ ] R3 路径前缀 + 规范化：10 条全部通过
- [ ] R4 路由优先级：6 条全部通过
- [ ] R5 安全不变量：5 条代码 review 验证
- [ ] R6 配置错误处理：6 条全部通过
- [ ] R6b Backends 注册：15 条全部通过
- [ ] R7 真实 Manager+Worker 集成测试：8 条全部通过

**总计 68 条用例。**

---

## 实现要点

### BackendRegistry 数据结构

```rust
pub struct BackendRegistry {
    backends: HashMap<String, Box<dyn Runtime>>,
    default_name: String,
    routes: Vec<RouteRule>,
}

impl BackendRegistry {
    /// 从配置构造，注册所有 backend
    pub fn from_config(cfg: &RuntimeConfig, workspace: &str) -> Self { ... }

    /// 路由解析：命令字符串 → backend
    fn resolve_command(&self, command: &str) -> &dyn Runtime { ... }

    /// 路由解析：路径 → backend（先规范化路径）
    fn resolve_path(&self, path: &str) -> &dyn Runtime { ... }
}
```

### AppleContainerRuntime 实现要点

```rust
pub struct AppleContainerRuntime {
    container_id: String,        // container run 后获得
    image: String,
    port: u16,
    ip: String,                  // container inspect 后获得
    memory: Option<String>,
    cpus: Option<u32>,
}

// 所有命令通过 container exec 在容器内执行
impl Runtime for AppleContainerRuntime {
    async fn execute_command(&self, cmd: &str, timeout: u64) -> Result<...> {
        let full = format!("container exec {} sh -c '{}'", self.container_id, escape(cmd));
        // 调 LocalRuntime 执行 container 命令
    }
    // 文件操作通过 container exec + cat/echo 在容器内完成
}
```

**启动流程（lazy 模式）：**

1. Worker 启动，BackendRegistry 创建所有 backend 对象
2. RemoteRuntime / LocalRuntime / SandboxRuntime 立即就绪
3. AppleContainerRuntime 标记为 "pending"
4. 第一次调用时，触发 `container run` 创建容器，记录 container_id
5. 后续调用通过 `container exec` 在已创建的容器内执行
6. Worker 关闭时调用 `container stop {container_id}` 清理
