# 看门狗双版本切换设计

> **状态：待定** — 设计思路记录，暂不实现。优先完成 A→B 自进化的生产化改造。

## 背景

A 改了自己的代码后，需要重启才能加载新代码。但 A 不能自己 kill 自己再启动（会断开）。需要一个"看门狗"来管理重启过程。

## 核心思路（用户提出）

A 合并新代码前，旧版本（A_old）是经过验证的、能跑的"安全基线"。直到 A_new 完全启动且自检通过，才杀掉 A_old。双版本并存只在切换那一刻存在。

## 三层架构

```
W = 看门狗（极简 bash，永不进化，不依赖 ion）
A_old = 当前运行版本（经过验证的安全基线）
A_new = 合并后的新版本（需要自检通过才替换 A_old）
```

## 流程

1. W 启动 A_old
2. A_old 接收任务 → 调 B 改代码 → CI 通过 → 合并 → 通知 W（写文件 /tmp/.ion-evolve-restart）
3. W 编译新代码
4. W 启动 A_new（不同端口）
5. A_new 自检（health check）
6. 自检通过 → W 杀 A_old → A_new 成为新的 A_old
7. 自检失败 → W 回滚 binary → A_old 继续跑

## 关键原则

- W 的代码永远不变（不参与进化）
- W 不依赖 ion / LLM / container
- W 有超时保护（A_new 启动失败 W 能检测到）
- W 有回滚能力（A_new 跑不起来，W 用 A_old 备份启动）
- 切换期间零停机（A_old 一直在服务）

## 后续实现

- `scripts/watchdog.sh`（极简 bash，~100 行）
- A 通知 W：`std::fs::write("/tmp/.ion-evolve-restart", "restart")`
- A 自检：`GET /health` 返回 ok
- 双版本端口管理：A_old=9000, A_new=9001（临时）

## 关联文档

- [SELF_EVOLUTION.md](./SELF_EVOLUTION.md) — A→B 自进化架构
- `scripts/evolve.sh` — 环境初始化
- `scripts/evolve-run.sh` — B 改代码 + CI + 合并
