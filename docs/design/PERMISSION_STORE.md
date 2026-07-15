# Stored-Decision 权限记忆 设计文档

> **状态：待定** — 用户选"always allow"后持久化，下次自动放行，不用反复确认。
>
> 对齐 pi 的 `permissions/providers/stored-decision.ts`。

---

## 何时使用这个文档

- 用户觉得每次工具调用都要确认很烦
- 想让"always allow"选项真正记住决策
- 想给 PermissionExtension 加持久化决策存储

**前置阅读**：[PERMISSION_SYSTEM.md](./PERMISSION_SYSTEM.md)

---

## 1. 问题

ION 当前的权限系统（`src/agent/permission_extension.rs`）：
- `PermissionRule` 有 `subject`/`pattern`/`decision`/`scope`
- `add_rule()` 可以手动加规则（通过 `extension_rpc`）
- `check()` 遍历规则做匹配

**缺失**：用户在 UI 确认权限时选了"always allow project"后，这个决策**不会被记住**——下次同一个操作还要再问。

pi 的 `stored-decision.ts` 解决了这个问题：把用户的授权决策持久化到 `settings.json`，下次自动放行。

## 2. 设计

### 2.1 存储位置

决策记录存到 `~/.ion/settings.json`（全局）和 `<project>/.ion/settings.json`（项目级），复用已有的 `PermissionRule` 结构，加一个 `source: "stored"` 标记。

```json
{
  "permissions": {
    "rules": [
      {
        "id": "perm_stored_xxx",
        "subject": "tool.execute",
        "pattern": "bash:git *",
        "decision": "allow",
        "scope": "project",
        "source": "stored",
        "createdAt": "2026-07-15T10:00:00Z"
      }
    ]
  }
}
```

**维度**（对齐 CONFIG_DIMENSIONS）：
- 全局 `~/.ion/settings.json` → 对所有项目生效（维度①）
- 项目级 `<project>/.ion/settings.json` → 仅当前项目（维度③）

### 2.2 数据结构

**文件**：`src/agent/permission_extension.rs`（现有文件改动）

```rust
/// 权限决策来源（区分手动配置 vs 用户运行时选择）
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum DecisionSource {
    /// 用户在 settings.json 里手动配的
    Config,
    /// 用户在 UI 确认时选"always allow"自动生成的
    Stored,
}

/// 给 PermissionRule 加 source 字段
pub struct PermissionRule {
    pub id: String,
    pub subject: String,
    pub pattern: String,
    pub decision: Decision,
    pub scope: Scope,
    pub source: DecisionSource,  // 新增
    pub created_at: Option<String>,  // 新增
}
```

### 2.3 存储决策的触发

当用户在 UI 确认权限时，UiSystem 返回用户选择。新增"always allow"选项：

```rust
pub enum UiPermissionResult {
    Allow,           // 允许一次
    Deny,            // 拒绝
    AlwaysAllowProject,  // 始终允许（项目级，新增）
    AlwaysDenyProject,   // 始终拒绝（项目级，新增）
}
```

用户选 `AlwaysAllowProject` 后：
1. 自动生成一条 `PermissionRule { source: Stored, scope: Project, decision: Allow }`
2. 写入 `<project>/.ion/settings.json`
3. 下次 `check()` 匹配到这条规则，自动放行

### 2.4 RPC 接口

```bash
# 列出所有存储的决策
ion rpc --session <sid> --method permission_list_stored

# 删除某条存储决策（撤销 always allow）
ion rpc --session <sid> --method permission_remove_stored \
  --params '{"id":"perm_stored_xxx"}'

# 清空所有存储决策
ion rpc --session <sid> --method permission_clear_stored
```

## 3. 改动文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/agent/permission_extension.rs` | PermissionRule 加 source 字段 + store_decision() / list_stored() / remove_stored() | ~80 |
| `src/agent/ui_system.rs` | UiPermissionResult 加 AlwaysAllowProject / AlwaysDenyProject | ~20 |
| `src/bin/ion_worker.rs` | 权限确认后调 store_decision | ~15 |
| `tests/permission_store_ci.sh` | CLI 测试 | ~60 |
| **总计** | | **~175** |

## 4. CLI 测试指南

### Group A：stored-decision 基本流程

```bash
# A1 模拟用户选 always allow → 决策被存储
ion rpc --session <sid> --method permission_store_decision \
  --params '{"subject":"tool.execute","pattern":"bash:git status","decision":"allow","scope":"project"}'
# 验证 settings.json 里出现 rule

# A2 下次同一操作自动放行
ion rpc --session <sid> --method permission_check \
  --params '{"subject":"tool.execute","input":{"command":"git status"}}'
# 验证返回 allow（不再问）

# A3 删除存储决策
ion rpc --session <sid> --method permission_remove_stored \
  --params '{"id":"perm_stored_xxx"}'
# 验证 settings.json 里 rule 消失
```

## 5. 并行开发注意事项

- **不依赖**其他 3 份文档，可独立并行开发
- 改动集中在 `permission_extension.rs` + `ui_system.rs` + `ion_worker.rs`
- 不改 Extension trait，不影响 hooks/扩展系统
- 测试用 `permission_store_ci.sh`，与 hooks_ci 互不干扰

## 6. 对标 pi

| 对比项 | pi | ION |
|--------|-----|-----|
| 存储位置 | settings.json | settings.json（对齐） |
| scope | session/project/global | project/global（session 不持久化） |
| 撤销方式 | slash `/permissions` 命令 | RPC `permission_remove_stored` |
| 自动过期 | 无 | 无（对齐） |
