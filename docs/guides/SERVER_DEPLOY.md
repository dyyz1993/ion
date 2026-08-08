# ION Linux 服务器部署指南

> **状态：已验证** — 2026-08-08 在腾讯云 CloudStudio 容器（Ubuntu 20.04, glibc 2.31）实战部署通过，含 CI 自动发布、单二进制安装、supervisord 守护、三种场景验证。

本文档讲的是：**如何把 ion 部署到一台 Linux 服务器上，让它常驻跑场景三（`ion serve`），崩溃自动重启、容器/机器重启自动拉起。**

如果你想配 runtime（local/remote/sandbox 执行后端），看 [DEPLOY_ARCH.md](./DEPLOY_ARCH.md)，那是另一个主题。

---

## 0. 谁该读这篇

- 想在内网/云服务器上跑 ion host 服务，供 RPC / 外部 UI 连接
- 想用 CI 自动发 Linux 二进制 Release，机器上一条 `curl` 命令安装
- 嫌 docker 部署重，想要单二进制零依赖

**前置技能**：会 ssh、会用编辑器、能看懂 JSON 和 shell。

---

## 1. 机器要求（很低）

| 维度 | 要求 | 备注 |
|------|------|------|
| 架构 | x86_64 | 当前 Release 只编了 x86_64，arm64 需另发 |
| glibc | **≥ 2.31** | Ubuntu 20.04+ / Debian 11+ / CentOS 9+ 都行 |
| 磁盘 | ~100MB | 二进制 49MB + 配置 + 会话增长 |
| 内存 | 17MB 起步 | idle 0（进程不常驻则 0），跑任务峰值 ~17MB |
| 外部依赖 | **零** | 不需要 Node/Python/openssl/docker |

**验证命令**（4 条）：
```bash
uname -m                    # 必须输出 x86_64
ldd --version | head -1     # 看到 2.31 或更高
which curl tar              # 两个都有
df -h ~                     # 剩余 > 100MB
```

---

## 2. CI 自动发布（一次性，仓库维护者做）

### 2.1 Release workflow

仓库根目录已有 `.github/workflows/release.yml`。触发方式：
- **手动**：GitHub Actions 页面 → Release → Run workflow（可填 version，默认走 Cargo.toml）
- **push tag `v*`**：自动触发

**为什么用 `container: ubuntu:20.04`**：直接用 `ubuntu-20.04` runner 会排队（GitHub 已 deprecate 该 runner）。改用 `ubuntu-22.04` runner + `container: ubuntu:20.04`，秒级起步且产出 glibc 2.31 二进制，向下兼容。

### 2.2 编译 aws-lc-sys 的三个坑（已踩透）

`aws-lc-sys` 被 `jsonschema` + `rmcp` 经 reqwest 0.13 → rustls 间接拉入，**无法在 Cargo.toml 层面回避**。在 ubuntu:20.04 容器里源码编译它，需要：

| 依赖 | 为什么需要 |
|------|----------|
| `cmake` + `golang-go` | 编译 AWS-LC C 代码 |
| `libclang-dev` + `clang` | bindgen 生成 Rust 绑定 |
| **`gcc-10` + update-alternatives** | **关键**：20.04 默认 gcc-9 有 [GCC#95189](https://gcc.gnu.org/bugzilla/show_bug.cgi?id=95189) memcmp bug，aws-lc-sys 在 `cc_builder.rs:872` 检测到后直接 panic 拒绝编译 |

workflow 里的依赖装法：
```yaml
- name: Install build deps (in 20.04 container)
  run: |
    apt-get update
    apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential pkg-config libssl-dev \
      cmake golang-go libclang-dev clang \
      gcc-10 g++-10
    update-alternatives --install /usr/bin/cc cc /usr/bin/gcc-10 100
    update-alternatives --install /usr/bin/c++ c++ /usr/bin/g++-10 100
    update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-10 100
    update-alternatives --install /usr/bin/g++ g++ /usr/bin/g++-10 100
env:
  AWS_LC_SYS_PREGENERATED_BINDINGS: "1"  # 双保险：优先用预生成绑定
```

### 2.3 install.sh（随 Release 打包）

`.github/assets/install.sh` 会被打进 tarball。它做的事：root 装 `/usr/local/bin`，非 root 装 `~/.local/bin` + PATH 检查 + 冒烟。

---

## 3. 服务器安装（每台机器一次，约 1 秒）

### 3.1 一条命令下载安装

```bash
# 拿最新 Release 的下载地址
ASSET=$(curl -s https://api.github.com/repos/dyyz1993/ion/releases/latest \
  | grep -o 'ion-x86_64[^"]*\.tar.gz' | head -1)

# 下载 + 解压 + 跑 install.sh
curl -fsSL -o /tmp/ion.tar.gz \
  "https://github.com/dyyz1993/ion/releases/latest/download/$ASSET" \
  && tar -xzf /tmp/ion.tar.gz -C /tmp \
  && bash /tmp/install.sh \
  && rm -f /tmp/ion /tmp/ion.tar.gz /tmp/install.sh
```

成功标志：
```
✅ ion 已安装到 /usr/local/bin/ion
✅ 冒烟通过：ion 0.1.0
```

### 3.2 配置（2 个文件）

**`~/.ion/config.json`**（手写）：
```json
{
  "default_provider": "zai",
  "default_model": "glm-5.2",
  "providers": {
    "zai": {
      "name": "zai",
      "api": "openai-completions",
      "base_url": "https://your-zai-proxy/v4",
      "models": [
        {"id": "glm-5.2", "name": "GLM-5.2", "reasoning": true, "context_window": 128000}
      ]
    }
  },
  "tier_models": {
    "max": "zai/glm-5.2",
    "pro": "zai/glm-5.2",
    "fast": "zai/glm-5.2"
  },
  "mcp_servers": {}
}
```

> ⚠️ **`tier_models` 很重要**：memory-agent 默认用 `fast` tier。如果 fast 指向的 provider 没余额/没配，会持续报错。建议三个 tier 都指向同一个有余额的 provider。

**`~/.ion/auth.json`**（用命令生成，权限自动 600）：
```bash
ion config set api-key "sk-你的真实key"
```

> ⚠️ **关键坑**：`ion config set api-key` 把 key 存到 `auth.json`，但 **ion 实际读的是 `config.json` 里 provider 的 `api_key` 字段**。如果 config.json 里 provider 那行是占位符，会报 401。修复方式：手动把 key 同步写进 `config.json` 的 `providers.X.api_key` 字段。

---

## 4. 三个场景怎么跑

### 场景 1：`ion -p`（单次执行，跑完退出）

```bash
echo "你的问题" | ion -p
# 或
ion -p "你的问题"
```

> ⚠️ **GLM-5.2 是 reasoning 模型，简单问题也要思考 30-120 秒**。脚本里套 `timeout` 别设太短（建议 ≥ 180s），否则会被误判为"挂死"。这是思考延迟，不是 bug。

### 场景 2：`ion --host "任务"`（临时 host + 事件泵到 stdout，跑完自动退）

```bash
ion --host "帮我看下当前目录有哪些文件"
```

适合冒烟测试、一次性编排任务。

### 场景 3：`ion serve`（常驻服务 + Unix socket，外部可连）—— 见下一节

---

## 5. 场景三守护：supervisord 接管（推荐）

### 5.1 为什么不用 systemd / nohup

| 方案 | 适用场景 | 局限 |
|------|---------|------|
| systemd | 物理机 / VM（Ubuntu/CentOS 全功能安装） | ❌ 容器里通常没 systemd（PID 1 是 dumb-init/tini） |
| nohup + 自动重启脚本 | 临时/快速 | ❌ 机器重启后不会自动拉起 |
| **supervisord** | **容器环境**（很多云开发环境/CI runner 都内置） | ✅ 容器原生进程管理器，开机自启 + 崩溃重启 |

**先确认你的环境用什么 init**：
```bash
ps -p 1 -o pid,cmd    # 看 PID 1 是什么
# systemd      → 用 systemd 方案（写 .service 文件）
# dumb-init/tini/supervisord → 容器环境，用 supervisord 方案
```

### 5.2 supervisord 接管步骤

**前置确认**：supervisord 在跑（很多容器自带），且它的配置文件在 `/etc/supervisord.conf`。

```bash
# 1. 备份原配置（必做，这个文件可能是厂商的）
cp /etc/supervisord.conf /etc/supervisord.conf.bak.$(date +%s)

# 2. 在配置末尾追加 [program:ion] 段
cat >> /etc/supervisord.conf << 'EOF'

[program:ion]
command=/usr/local/bin/ion serve
directory=/root
environment=HOME="/root"
autostart=true
autorestart=true
numprocs=1
redirect_stderr=true
startretries=10
startsecs=10
stdout_logfile=/var/log/ion-serve.log
stdout_logfile_maxbytes=10MB
stdout_logfile_backups=3
stopasgroup=true
killasgroup=true
EOF

# 3. 找 supervisord PID，发 SIGHUP 让它 reload
SUPER_PID=$(pgrep -f supervisord | head -1)
kill -HUP $SUPER_PID

# 4. 等 startsecs（10s）后验证
sleep 12
ls -la ~/.ion/host.sock    # socket 应该存在
ion rpc --method list_workers   # RPC 应该返回 JSON
```

### 5.3 验证已被接管

```bash
# ion 的 PPID 应该是 supervisord 的 PID
ps -eo pid,ppid,cmd | awk '$2 ~ /\/usr\/local\/bin\/ion/

# 假设 supervisord PID 是 18，ion 的 PPID 应该 = 18
ps -eo pid,ppid,cmd | grep "/usr/local/bin/ion serve" | grep -v grep
# 应输出类似：806646  18  ... /usr/local/bin/ion serve
```

### 5.4 验证崩溃自动重启

```bash
ION_PID=$(ps -eo pid,cmd | awk '$2=="/usr/local/bin/ion" && $3=="serve" {print $1; exit}')
kill -9 $ION_PID
sleep 3
# supervisord 应该 1-3 秒内拉起新进程，新 PID 不同
ps -eo pid,etime,cmd | grep "/usr/local/bin/ion serve" | grep -v grep
```

### 5.5 supervisord 方案的局限

- **`supervisorctl` 可能不可用**：如果原配置没有 `[unix_http_server]`/`[supervisorctl]` 段，supervisorctl 连不上。手动操作只能 `kill -HUP <supervisord-pid>` reload，或 `kill <ion-pid>` 触发重启。
- **reload 会重启所有 program**：SIGHUP 时 sshd/rsyslog 等都会跟着重启一下（正常行为，几秒内完成）。
- **修改 `[program:ion]` 后要 SIGHUP**：直接改文件不会生效，必须 `kill -HUP <supervisord-pid>`。

### 5.6 systemd 方案（物理机/VM 用这个）

如果你的机器是真正的 systemd 系统（`ps -p 1` 显示 systemd），用这个：

```bash
cat > /etc/systemd/system/ion.service << 'EOF'
[Unit]
Description=ION Agent Orchestration Host
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/ion serve
Restart=on-failure
RestartSec=5s
Environment=HOME=/root
WorkingDirectory=/root
StandardOutput=journal
StandardError=journal
ExecStartPre=/bin/rm -f %h/.ion/host.sock

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now ion
systemctl status ion
```

---

## 6. 日常运维速查

```bash
# 查 ion 进程（注意：别用宽泛的 pkill -f ion，会误杀含 "ion" 字样的进程如 LogiOptionsPlus）
ps -eo pid,ppid,etime,rss,cmd | grep "/usr/local/bin/ion" | grep -v grep

# RPC 操作（走 socket ~/.ion/host.sock）
ion rpc --method list_workers
ion rpc --method list_sessions
ion rpc --method get_overview
ion rpc --method create_worker --params '{"agent":"build","message":"你的任务"}'

# 订阅事件流
ion subscribe

# 看日志
tail -f /var/log/ion-serve.log        # supervisord 方案
journalctl -u ion -f                  # systemd 方案

# 手动重启 ion（supervisord/systemd 都会自动拉起）
kill -9 $(pgrep -f "/usr/local/bin/ion serve")

# 升级 ion（覆盖安装）
ASSET=$(curl -s https://api.github.com/repos/dyyz1993/ion/releases/latest | grep -o 'ion-x86_64[^"]*\.tar.gz' | head -1)
curl -fsSL -o /tmp/ion.tar.gz "https://github.com/dyyz1993/ion/releases/latest/download/$ASSET" \
  && tar -xzf /tmp/ion.tar.gz -C /tmp && bash /tmp/install.sh && rm -f /tmp/ion /tmp/ion.tar.gz /tmp/install.sh
# 升级后重启 ion 进程（supervisord 会自动拉起新版本）
kill -9 $(pgrep -f "/usr/local/bin/ion serve")
```

---

## 7. 踩坑记录（实战总结）

### 坑 1：gcc-9 的 memcmp bug 导致 aws-lc-sys 编译失败
- 现象：CI 编译时 `cc_builder.rs:872 panic, ERROR: (空)`
- 根因：ubuntu:20.04 默认 gcc-9 有 [GCC#95189](https://gcc.gnu.org/bugzilla/show_bug.cgi?id=95189)，aws-lc-sys 检测到后拒绝编译
- 修复：装 gcc-10 + update-alternatives 设为默认

### 坑 2：config.json 里 provider 的 api_key 是占位符 → 401
- 现象：`401 AuthError: Invalid API key`
- 根因：`ion config set api-key` 存到 auth.json，但 ion 读的是 config.json 里 provider 的 api_key 字段
- 修复：手动把真实 key 写进 `config.json` 的 `providers.X.api_key`

### 坑 3：memory-agent 报 CreditsError（余额不足）
- 现象：`CreditsError: Insufficient balance`（指向 opencode provider）
- 根因：tier_models 的 fast 指向了没余额的 provider
- 修复：把 `tier_models` 全部指向有余额的 provider（如 `zai/glm-5.2`）

### 坑 4：把 reasoning 模型的思考延迟误判为"挂死"
- 现象：`ion -p` 跑 30/90 秒没输出，被 timeout 杀掉
- 根因：GLM-5.2 是 reasoning 模型，简单问题也思考 30-120 秒，期间流式不吐 token
- 修复：不是 bug，给足 timeout（≥180s）。可用 `ION_STREAM_DEBUG=1` 确认 POST 发出且收到 chunk

### 坑 5：宽泛 `pkill -f ion` 误杀 ssh 会话
- 现象：ssh 连接突然断开（exit 255）
- 根因：pkill -f "ion" 匹配到了 ssh 命令行里含 "ion" 的字符串
- 修复：永远用精确 PID（`kill $(pgrep -f "/usr/local/bin/ion serve")`），**禁止** `pkill -f "ion"`（详见 AGENTS.md CI 脚本进程清理规范）

### 坑 6：容器里 systemctl 失灵但 systemctl 命令存在
- 现象：`systemctl status` 报 `System has not been booted with systemd as init system`
- 根因：容器装了 systemctl 二进制，但 PID 1 不是 systemd
- 修复：`ps -p 1 -o cmd` 确认真正的 init（dumb-init/supervisord），用对应方案

### 坑 7：supervisord.conf 没 include conf.d
- 现象：把配置放到 `/etc/supervisor/conf.d/ion.conf`，supervisord 不加载
- 根因：主配置文件没有 `[include] files = /etc/supervisor/conf.d/*.conf` 段
- 修复：直接把 `[program:ion]` 段追加到主配置 `/etc/supervisord.conf` 末尾

---

## 8. 资源占用实测（2026-08-08, tx 容器）

| 项目 | 数值 |
|------|------|
| 二进制 `/usr/local/bin/ion` | 49 MB |
| `~/.ion`（config + sessions + memory DB） | 296 KB（起步，随使用增长）|
| 磁盘总需求 | ~50 MB |
| ion serve RSS（常驻） | ~17 MB |
| ion -p 单次任务峰值 RSS | ~17 MB |
| idle（进程不跑时） | 0 |

**二进制依赖**：`ldd` 显示 7 条 shared lib，全是 glibc 自带（libgcc/libm/libc/ld-linux），`not found: 0` —— 真正的零外部依赖。

---

## 9. 回滚

### 卸载 ion
```bash
# 停进程
kill $(pgrep -f "/usr/local/bin/ion serve")
# 从 supervisord 移除（注释掉 [program:ion] 段后 SIGHUP）
sed -i '/^\[program:ion\]/,/^\[/d' /etc/supervisord.conf
kill -HUP $(pgrep -f supervisord | head -1)
# 删二进制和配置
rm -f /usr/local/bin/ion
rm -rf ~/.ion
```

### 恢复 supervisord 原配置
```bash
cp /etc/supervisord.conf.bak.* /etc/supervisord.conf
kill -HUP $(pgrep -f supervisord | head -1)
```
