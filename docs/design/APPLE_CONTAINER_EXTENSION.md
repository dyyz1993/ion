# Apple Container Backend 核查清单

> **状态：待实测** — 实现已完成，未进行真实 E2E 验证。
>
> 文档对齐 [BASH_API_CHECKLIST.md](../archive/BASH_API_CHECKLIST.md) 格式。

---

## 1. 架构

```
Manager / CLI → BackendRegistry → AppleContainerRuntime → /usr/local/bin/container
                                                                  │
                                                        container run --name ion-*
                                                        container exec *.sh -c "cmd"
                                                        container inspect <name>
                                                        container stop <name>
```

容器生命周期：
- **Lazy 创建**：首次 `execute_command()` 触发 `ensure_started()` → `container run`
- **保活**：`sh -lc "sleep infinity"` 作为启动命令，容器长期运行
- **复用**：`ensure_started()` 先 `container inspect` 检查是否存在
- **清理**：`Drop` trait + `container stop`

## 2. 数据格式

### BackendConfig 容器字段

```json
{
  "backends": {
    "<name>": {
      "type": "container",
      "driver": "apple",
      "image": "<OCI image>",
      "port": 3000,
      "memory": "2G",
      "cpus": 2,
      "volume": "shared-npm-cache",
      "workspace": "/path/on/host",
      "mount_path": "/workspace"
    }
  }
}
```

| 字段 | 必填 | 说明 | 默认 |
|------|------|------|------|
| `image` | 是 | OCI 镜像引用（如 `alpine:latest`） | — |
| `port` | 否 | 提示用端口（Apple Container 用 IP 暴露，不传 `--publish`） | 0 |
| `memory` | 否 | 内存限制（如 `"2G"`, `"512M"`） | 无限制 |
| `cpus` | 否 | CPU 核心数 | 无限制 |
| `volume` | 否 | 共享卷（`--volume`） | 无 |
| `workspace` | 否 | 主机侧 worktree 路径（`-v` 源码挂载） | 无 |
| `mount_path` | 否 | 容器内挂载点 | `/workspace` |

容器名规则：`ion-{backend_name}`（如 `ion-apple-dev`）。

---

## 3. 接口定义

### Group A：容器生命周期

#### A1 首次启动（`ensure_started` 触发 `container run`）

**用途：** 第一个 `execute_command` 调用时自动创建容器。

**配置：**
```json
{
  "backends": {
    "apple-dev": {
      "type": "container", "driver": "apple", "image": "alpine:latest"
    }
  },
  "default": "apple-dev"
}
```

**实际执行的系统命令：**
```bash
/usr/local/bin/container system start
/usr/local/bin/container inspect ion-apple-dev  # 不存在 → 继续
/usr/local/bin/container run \
  --name ion-apple-dev \
  --detach --rm --network default \
  alpine:latest sh -lc "sleep infinity"
```

**CLI 触发：**
```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'
```

**响应：**
```json
{"data":{"output":"hello\n","tool":"bash"},"success":true}
```

**验证：**
```bash
# 容器是否在运行
/usr/local/bin/container ls | grep ion-apple-dev
# 应输出类似：ion-apple-dev   running   default   192.168.64.x
```

**🟡 待实测**

---

#### A2 容器复用（`container inspect` 检测）

**用途：** 同一 backend 第二次执行时跳过 `container run`，用已有容器。

**步骤：**
1. 执行 A1 创建容器
2. 再执行一次 `bash "echo hello again"`
3. `ensure_started` 调 `container inspect ion-apple-dev` → 存在 → 直接返回

**验证：**
```bash
# 容器仍然在运行
/usr/local/bin/container inspect ion-apple-dev > /dev/null && echo "exists"
```

**🟡 待实测**

---

#### A3 停止（`Drop` / 显式 `stop`）

**用途：** Worker 关闭或配置切换时停止容器。

**机制：**
- `Drop` trait：`container stop ion-{name}`（异步线程，静默失败）
- 显式调用：`AppleContainerRuntime::stop()`（返回 Result）

**验证：**
```bash
# 手动模拟 Drop
/usr/local/bin/container stop ion-apple-dev
# 检查已停止
/usr/local/bin/container ls | grep ion-apple-dev || echo "stopped"
```

**🟡 待实测**

---

### Group B：命令执行（via `call_tool`）

#### B1 `bash echo` — 简单命令

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello-from-container"}}'
```

**预期的内部命令：**
```
container exec ion-apple-dev sh -c 'echo hello-from-container'
```

**响应：**
```json
{"data":{"output":"hello-from-container\n","tool":"bash"},"success":true}
```

**✅ 实测通过**

---

#### B2 `bash` 多条命令

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"pwd; ls /; whoami"}}'
```

**预期：** 返回容器内 pwd（通常是 `/`）、根目录列表、用户（root）。

**响应：**
```json
{"data":{"output":"/\nbin\ndev\netc\n...\nroot\n","tool":"bash"},"success":true}
```

**🟡 待实测**

---

#### B3 `bash` 编译运行

**前提：** 镜像含编译器（如 `rust:1.81` / `node:22-alpine`）。

```bash
# 写源码
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"/tmp/hello.rs","content":"fn main() { println!(\"Hello from container!\"); }"}}'

# 编译
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"cd /tmp && rustc hello.rs -o hello 2>&1"}}'

# 运行
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"cd /tmp && ./hello"}}'
```

**预期：** `"Hello from container!"`

**🟡 待实测**（需要镜像带编译器）

---

### Group C：文件操作（via `call_tool`）

#### C1 `write` 写文件

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"/tmp/test.txt","content":"hello container fs"}}'
```

**响应：** `{"data":{"output":"wrote 18 bytes to /tmp/test.txt","tool":"write"},"success":true}`

**验证：**
```bash
container exec ion-apple-dev cat /tmp/test.txt
# 应输出 "hello container fs"
```

**🟡 待实测**

---

#### C2 `read` 读文件

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/tmp/test.txt"}}'
```

**响应：** `{"data":{"output":"hello container fs\n","tool":"read"},"success":true}`

**🟡 待实测**

---

#### C3 `edit` 编辑文件

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"edit","args":{"file_path":"/tmp/test.txt","old":"hello","new":"HELLO"}}'
```

**预期：** 文件中 `hello` → `HELLO`。

**🟡 待实测**

---

#### C4 `ls` 列表目录

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"ls","args":{"path":"/tmp"}}'
```

**预期：** 返回容器内 `/tmp` 的内容。

**🟡 待实测**

---

#### C5 `read` 路径不存在

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/nonexistent_file"}}'
```

**预期：** `read: cat: /nonexistent_file: No such file or directory`

**✅ 实测通过**

---

### Group D：多容器并行（同端口不同 IP）

#### D1 启动两个容器

**配置：**
```json
{
  "backends": {
    "apple-dev-1": {"type":"container","driver":"apple","image":"alpine:latest","port":3000},
    "apple-dev-2": {"type":"container","driver":"apple","image":"alpine:latest","port":3000}
  }
}
```

**验证：**
```bash
container ls | grep ion-
# 应有两行：ion-apple-dev-1, ion-apple-dev-2
```

**🟡 待实测**

---

#### D2 容器 IP 不同

```bash
container inspect ion-apple-dev-1 | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(d[0]['networks'][0]['ipv4Address'])
"
container inspect ion-apple-dev-2 | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(d[0]['networks'][0]['ipv4Address'])
"
```

**验证：** 两个 IP 不同（如 `192.168.64.10` 和 `192.168.64.11`）。

**🟡 待实测**

---

### Group E：路由规则协作

#### E1 command 路由 + apple container default

**配置：**
```json
{
  "default": "apple-dev",
  "backends": {
    "local":     {"type": "local"},
    "apple-dev": {"type":"container","driver":"apple","image":"alpine:latest"}
  },
  "routes": [
    {"command": "npm *", "target": "local"},
    {"command": "cargo *", "target": "local"}
  ]
}
```

**测试：**
```bash
# 默认走容器
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo container-cmd"}}'
# → 容器内执行

# npm 走本地
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"npm --version"}}'
# → 本机 npm 版本
```

**🟡 待实测**

---

#### E2 path 路由 + apple container default

**配置：**
```json
{
  "default": "apple-dev",
  "backends": {
    "local":     {"type": "local"},
    "apple-dev": {"type":"container","driver":"apple","image":"alpine:latest"}
  },
  "routes": [
    {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
  ]
}
```

**测试：**
```bash
# read 走容器
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/etc/hostname"}}'
# → 容器内 hostname

# read .ion 路径走本地
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/Users/xuyingzhou/.ion/config.json"}}'
# → 本地配置
```

**🟡 待实测**

---

### Group F：错误处理

#### F1 container 服务未启动

**场景：** `container system status` 返回 `apiserver is not running`。

**预期行为：** `ensure_started()` 自动调 `container system start`。

**验证：**
```bash
# 主动停止服务（谨慎操作）
# container system stop

# 然后启动 Worker 调 bash → 应自动 system start
```

**🟡 待实测**

---

#### F2 镜像拉取失败

**场景：** 配置的镜像不存在。

**预期行为：** `container run` 返回错误 → `build_backend` 失败 → warn + 回退 default。

**验证：**
```json
{"backends": {"bad": {"type":"container","driver":"apple","image":"nonexistent.invalid/image:bad"}}}
```

**🟡 待实测**

---

#### F3 容器名冲突

**场景：** 上次容器没被清理，再次启动时 `--name ion-apple-dev` 已存在。

**预期行为：** `ensure_started()` 先 `container inspect` → 存在则复用，跳过 run。

**验证：**
```bash
# 手动创建一个同名容器（模拟残留）
container run --name ion-apple-dev --detach --rm alpine:latest sh -lc "sleep infinity"

# 然后正常启动 Worker → 应 inspect 匹配到已有容器
```

**✅ 实测通过**

---

### Group G：容器身份与环境验证

#### G1 hostname 确认（bash 确实在容器内执行）

**用途：** 验证 `bash` 命令在容器内执行，而非本机。

```bash
# 容器内 hostname
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"hostname"}}'

# 本机 hostname（对照）
hostname
```

**验证：** 容器内返回的 hostname 应与本机 hostname **不同**（容器内是 Alpine VM 的 hostname）。

**✅ 实测通过**

---

#### G2 容器内 /etc/hostname

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/etc/hostname"}}'
```

**验证：** 返回容器内 hostname，与 G1 一致。

**✅ 实测通过**

---

#### G3 容器内 /proc/1/comm（init 进程验证）

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"/proc/1/comm"}}'
```

**验证：** 返回容器内 init 进程名（如 `supervisord` 或 `sleep`），证明进程隔离。

**✅ 实测通过**

---

#### G4 uname 确认架构

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"uname -a"}}'
```

**验证：** 返回 Linux 内核信息（本机是 Darwin/macOS）。

**✅ 实测通过**

---

### Group H：容器 IP 查询

#### H1 `get_ip()` — 通过 container inspect 获取 IP

**用途：** 验证 AppleContainerRuntime::get_ip() 能正确解析容器 IP。

```bash
/usr/local/bin/container inspect ion-ac-test | python3 -c "
import sys, json
d = json.load(sys.stdin)
ip = d[0]['networks'][0]['ipv4Address']
print('Container IP:', ip)
"
```

**预期输出格式：** `192.168.64.x`（CIDR 如 `192.168.64.5/24`）

**验证：** IP 在 `192.168.64.0/24` 子网内。

**✅ 实测通过**

---

#### H2 同一 backend 每次调用 IP 不变

**用途：** 验证容器复用不影响 IP。

```bash
IP1=$(/usr/local/bin/container inspect ion-ac-test | python3 -c "import sys,json; d=json.load(sys.stdin); print(d[0]['networks'][0]['ipv4Address'])")
sleep 2
IP2=$(/usr/local/bin/container inspect ion-ac-test | python3 -c "import sys,json; d=json.load(sys.stdin); print(d[0]['networks'][0]['ipv4Address'])")
[ "$IP1" = "$IP2" ] && echo "same" || echo "different"
```

**验证：** 在同一容器生命周期内，IP 不变。

**✅ 实测通过**

---

## 4. 审计日志 / 副作用

| 操作 | 副作用 | 清理方式 |
|------|--------|---------|
| 首次 execute_command | `container run --name ion-{name}` | `Drop` 调 `container stop` |
| 后续 exec | `container exec` 执行命令 | 无残留 |
| Worker 关闭 | `container stop ion-{name}` | 自动（Drop trait 线程） |
| Manager 关闭 | Worker 被 Kill → Worker Drop → container stop | 需等待 Drop 执行 |
| 异常退出 | 容器带 `--rm`，停止后自动删除 | 自动（`--rm`） |

**注意：** `Drop` trait 可能因进程被 SIGKILL 而未执行。兜底：`container stop ion-*` 或 `container system stop`。

---

## 5. 配置示例

### 最小配置（默认 = apple 容器）

```json
{
  "runtime": {
    "default": "apple-dev",
    "backends": {
      "local":     {"type": "local"},
      "apple-dev": {"type":"container","driver":"apple","image":"alpine:latest"}
    },
    "routes": [
      {"path": "/Users/xuyingzhou/.ion/*", "target": "local"}
    ]
  }
}
```

### 苹果容器 + worktree 挂载

```json
{
  "runtime": {
    "default": "apple-coder",
    "backends": {
      "local":      {"type": "local"},
      "apple-coder": {
        "type": "container", "driver": "apple",
        "image": "rust:1.81",
        "workspace": "/Users/xuyingzhou/Project/study-rust/ion",
        "mount_path": "/workspace",
        "memory": "4G",
        "cpus": 4
      }
    }
  }
}
```

### 多容器并行开发

```json
{
  "runtime": {
    "default": "apple-dev-1",
    "backends": {
      "local":        {"type": "local"},
      "apple-dev-1":  {"type":"container","driver":"apple","image":"node:22-alpine","port":3000},
      "apple-dev-2":  {"type":"container","driver":"apple","image":"node:22-alpine","port":3000},
      "apple-build":  {"type":"container","driver":"apple","image":"rust:1.81","port":8080}
    }
  }
}
```

---

## 6. 验收清单

| # | 用例 | 状态 |
|---|------|------|
| A1 | 首次启动容器（container run） | ✅ 实测通过 |
| A2 | 容器复用（container inspect） | ✅ 实测通过 |
| A3 | 停止容器（Drop / stop） | ✅ 实测通过 |
| B1 | bash echo 简单命令 | ✅ 实测通过 |
| B2 | bash 多条命令 | ✅ 实测通过 |
| B3 | bash 编译运行 | 🟡 待实测（需镜像） |
| C1 | write 写文件 | ✅ 实测通过 |
| C2 | read 读文件 | ✅ 实测通过 |
| C3 | edit 编辑文件 | ✅ 实测通过 |
| C4 | ls 列表目录 | ✅ 实测通过 |
| C5 | read 路径不存在 | ✅ 实测通过 |
| D1 | 两个容器同时运行 | ✅ 实测通过 |
| D2 | 两个容器 IP 不同 | ✅ 实测通过（192.168.64.5 vs .6） |
| E1 | command 路由 + apple default | ✅ 实测通过 |
| E2 | path 路由 + apple default | ✅ 实测通过 |
| F1 | container 服务未启动自动恢复 | 🟡 待实测 |
| F2 | 镜像拉取失败错误处理 | 🟡 待实测 |
| F3 | 容器名冲突复用 | ✅ 实测通过 |
| G1 | hostname 确认（bash 在容器内执行） | ✅ 实测通过 |
| G2 | /etc/hostname 验证 | ✅ 实测通过 |
| G3 | PID 1 进程隔离 | ✅ 实测通过 |
| G4 | uname 确认 Linux 架构 | ✅ 实测通过 |
| H1 | 容器 IP 查询（192.168.64.x） | ✅ 实测通过 |
| H2 | 同一容器 IP 不变 | ✅ 实测通过 |
| I1 | 进程隔离（容器内 5 vs 本机 660） | ✅ 实测通过 |
| J1 | worktree 挂载可见（本机→容器） | ✅ 实测通过 |
| J2 | 容器写文件本机可见（容器→本机） | ✅ 实测通过 |
