# 看门狗安全升级（单实例滚动重启）

> **状态：已验证** — 3 种故障场景全部验证通过（编译失败 / 运行时崩溃 / 进程卡死）。

## 设计决策：单实例，不用蓝绿

经过业界调研（nginx SIGUSR2 / systemd socket activation / Erlang hot code swap），**选择单实例滚动重启**，理由：

| 维度 | 双版本 (蓝绿) | 单实例 (滚动重启) |
|------|-------------|-----------------|
| AF_UNIX socket | ❌ 两个进程不能绑同一路径 | ✅ 继承已绑定的 FD |
| Worker 孤儿 | ❌ 新实例是不同 PID，旧 worker 变孤儿 | ✅ PID 不变 |
| 内存 | ❌ 切换瞬间 2x | ✅ 一套 |
| macOS binary 覆盖 | ❌ ETXTBSY | ✅ backup → compile → replace |
| 复杂度 | ❌ socket 仲裁 + health 切换 + 回滚 | ✅ 一个进程 |

## 架构

```
watchdog.sh (常驻 bash，永不进化)
    │
    ├── 每 5s 心跳：ion rpc --method health
    │   ├── 响应 ok → 正常，重置失败计数
    │   └── 超时/失败 → 失败计数 +1
    │       └── 连续 3 次失败 → 判定死亡
    │
    ├── 进程死亡（PID 不在 或 心跳 3 次失败）
    │   ├── kill -KILL 卡死的进程
    │   ├── 从 target/{debug,release}/.backups/ 恢复最近备份
    │   └── 重启 ion serve → health check
    │
    └── restart signal (/tmp/.ion-evolve-restart)
        ├── 备份当前 binary
        ├── cargo build (自动检测 debug/release)
        ├── 优雅停止 A_old (SIGTERM → 10s → SIGKILL)
        ├── 启动 A_new
        ├── health check (真实 RPC，不是 PID 检查)
        ├── 通过 → 切换完成，更新备份
        └── 失败 → 从备份恢复 → 重启 A_old
```

## 使用方式

```bash
# 一次性升级（A 合并新代码后手动触发）
ION_BIN=target/debug/ion bash scripts/watchdog.sh --upgrade

# 常驻监控（后台运行，自动检测崩溃 + restart signal）
ION_BIN=target/debug/ion bash scripts/watchdog.sh --monitor
```

A 在合并代码后通知 watchdog：
```bash
ion rpc --method request_restart --params '{}'
# 写入 /tmp/.ion-evolve-restart sentinel 文件
# watchdog --monitor 检测到后自动走升级流程
```

## 两种 RPC

| RPC | 级别 | 返回 | 用途 |
|-----|------|------|------|
| `health` | Manager | `{"status":"ok","workers":N,"version":"X"}` | 心跳检测，<10ms |
| `request_restart` | Manager | `{"notified":true,"file":"/tmp/.ion-evolve-restart"}` | A 通知 watchdog 升级 |

## 三种故障场景验证（2026-07-23）

| 场景 | 模拟方式 | 检测时间 | 恢复动作 | 结果 |
|------|---------|---------|---------|------|
| **编译失败** | 注入语法错误 | 即时 | A_old 原封不动 | ✅ PID 不变 |
| **启动崩溃** | `std::process::exit(1)` in main | ~10s | health 失败 → 从备份恢复 binary → 重启 | ✅ health 恢复 ok |
| **进程卡死** | `kill -STOP`（冻结） | ~20s（3 次心跳） | kill -KILL → 从备份恢复 → 重启 | ✅ PID 更新，health 恢复 |

## 关键设计点

1. **心跳不是 PID 检查** — PID 活着不代表进程能响应（死锁/OOM/hang）。每 5 秒发真实 health RPC，连续 3 次超时才判定死亡
2. **心跳带 timeout 3s** — frozen 进程会导致 `ion rpc` 无限等待。`timeout 3` 防止 watchdog 自己被拖死
3. **backup 路径跟 ION_BIN 一致** — `target/debug/.backups/` vs `target/release/.backups/`，自动从 `ION_BIN` 路径推导
4. **编译 profile 自动检测** — `ION_BIN` 路径含 `debug` → `cargo build`，含 `release` → `cargo build --release`
5. **优雅停止** — SIGTERM → 等待 10 秒 → SIGKILL，给 session 落盘时间

## 相关文件

| 文件 | 作用 |
|------|------|
| `scripts/watchdog.sh` | 看门狗脚本（纯 bash，~380 行） |
| `src/bin/ion.rs` | `health` + `request_restart` Manager 级 RPC |
| `src/bin/ion_worker.rs` | Worker 级 health RPC（内部使用） |

## 后续（暂不实现）

- **execve() 零停机重启** — 当前有 ~3-5 秒 socket 不可用窗口。用 FD 继承 + execve() 可以做到 0ms，但需要改 Rust 内核（signal handler + FD 传递），复杂度高
- **launchd/systemd 集成** — 让外部 init 系统监督 watchdog 自身（防 watchdog 被杀）
