# 看门狗双版本切换

> **状态：已实现** — `scripts/watchdog.sh` 已完成。

## 架构

```
W = 看门狗（scripts/watchdog.sh，纯 bash，永不进化）
A_old = 当前运行的 ion（安全基线）
A_new = 合并新代码后编译的 ion（自检通过才替换）
```

## 并发设计

Watchdog 本身是**后台并发运行**的：
- W 后台监控 A_old（每 5 秒检查一次）
- A_old 可以同时并发跑多个 B（spawn_worker × N）
- 当检测到 restart signal 时，W **后台编译 A_new**（不阻塞 A_old）
- A_new 启动在**不同端口**（9001），不影响 A_old（9000）
- 切换时 A_old 一直在服务（零停机）

## 流程

```
1. W 启动 A_old（ion serve --port 9000）
2. A_old 并发跑任务（spawn_worker × N）
3. A_old 合并新代码 → touch /tmp/.ion-evolve-restart
4. W 检测到信号 → 后台编译 A_new（不阻塞 A_old）
5. W 启动 A_new（端口 9001）
6. W 对 A_new 做 health check（5 秒存活检查）
7. 通过 → W 杀 A_old → A_new 成为新的 A_old
8. 失败 → W 回滚 binary → A_old 继续跑
```

## 关键特性

- **零停机**：A_old 在切换期间一直服务
- **自动回滚**：A_new 启动失败 → 回滚到 backup binary
- **自动恢复**：A_old 崩溃 → W 从 backup 重启
- **超时保护**：编译超时 600s / 健康检查超时 60s

## 使用

```bash
# 启动 watchdog（后台）
bash scripts/watchdog.sh &

# A_old 正常跑任务
# 当 A 合并新代码后：
touch /tmp/.ion-evolve-restart
# Watchdog 自动处理切换
```

## 关联文档

- [SELF_EVOLUTION.md](./SELF_EVOLUTION.md) — A→B 自进化架构
- `scripts/watchdog.sh` — 实际实现（~150 行 bash）
