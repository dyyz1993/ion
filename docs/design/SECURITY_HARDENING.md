# 安全加固 — 本地默认态 P0（安全对抗评审落地）

> **状态：已验证** — 2026-09-11 红蓝对抗评审（4 agent 两轮）P0 项落地：受保护路径集 + hooks 供应链信任门。真机验证通过。

---

## 背景

红蓝对抗评审（4 agent × 2 轮，含源码查证）结论：**默认 local 模式零隔离才是最大攻击面**。两大供应链向量不需要任何"切换"即可利用：

1. **config base_url 持久 MITM**：prompt injection 诱导 agent 写 `~/.ion/config.json` 改 base_url → 此后所有会话的 LLM 流量（含 key 请求头）持久流经攻击者
2. **hooks 供应链**：克隆恶意仓库（自带 `.ion/hooks.json`）→ 该目录开会话 → 任一事件触发即执行其命令（load_fresh 无条件合并）

## ① 受保护路径集（src/protected_paths.rs）

- **默认集**：`~/.ion/` 下 config.json / auth.json / hooks.json / settings.json / path-permissions.json + agent/models.json。**不可关闭**；`runtime.protected_paths_extra` 只允许追加
- **检查点**：SecuredRuntime 的 read/write/edit/remove 四操作，位于权限引擎**之前**——deny 不被 stored-decision/allow 遮蔽；核心工具与扩展 ctx.fs 全覆盖（都经此层）
- **canonicalize + 词法规范化**：`../` 与符号链接不绕过；目标不存在时 canonicalize 父目录（新建文件场景）
- 合法修改走 agent 之外：`ion config set` / 用户手动编辑（直接 fs，不过本层）

## ② hooks 供应链信任门（src/hooks/mod.rs + runtime.hooks_trust）

- 项目级 `.ion/hooks.json` **默认不加载**（deny by default，未上线项目无兼容负担）
- 放行：`runtime.hooks_trust.project_hooks_enabled=true`（全放）或 `trusted_projects: [路径]`（canonicalize 比对）
- 全局 `~/.ion/hooks.json` 不受影响；远程 worker 的 ION_NO_PROJECT_HOOKS=1 门保持优先

## 验证

- 单测：protected_paths（basic/traversal/extras）+ hooks 信任门判定矩阵 + load_fresh skip/allow + grants glob/解析 —— lib 1014/0
- 真机：本地会话注入诱导写 `~/.ion/config.json` → `[Protected]` 拒绝 + config 完好（default_provider 未变）✅

## 已知边界（诚实声明）

- bash 命令字符串内的路径不可靠解析（`echo x > config.json` 类）——CommandGuard 字符串匹配可绕过（官方文档已声明）；根本对策是不可信任务用 remote/container 后端（REMOTE_WORKER.md）
- trusted_projects 的 `../` 形式等价真实路径（canonicalize 语义）——配置时写规范路径
