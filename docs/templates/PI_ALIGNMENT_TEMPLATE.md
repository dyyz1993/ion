# pi {模块名} 对齐文档

> **状态：{开发中 / 已完成 / 待定}** — {一句话说明对齐进度}。
>
> **术语约定**：pi 的扩展系统叫 Extension（不叫 Plugin）。本文统一使用 extension 术语。

---

## 何时使用这个模板

调研 pi 某项能力、规划 ION 对齐方案时使用。一份对齐文档应覆盖：pi 现状 → ion 已有能力 → 差异点 → 对齐方案 → 实施进度。

**触发时机**：见 [AGENTS.md §文档规范-模板触发时机](../../AGENTS.md)。

**参考样本**：
- [docs/design/PI_RPC_ALIGNMENT.md](../design/PI_RPC_ALIGNMENT.md) — pi RPC CLI 对齐文档

---

## 一、背景

{为什么需要这个对齐？ion 当前缺什么？pi 有什么？}

**pi 源码位置**：`/Users/xuyingzhou/Project/temporary/pi-momo-fork/`
- 相关模块：`packages/{ai/rpc/session/agent}/src/{file}`

---

## 二、pi 当前状态

### 2.1 pi 的能力清单

| 能力 | pi 实现位置 | 说明 |
|------|------------|------|
| {能力 1} | `packages/xxx/src/yyy.ts:L1-L10` | {说明} |
| {能力 2} | `packages/xxx/src/yyy.ts:L20-L50` | {说明} |

### 2.2 pi 的关键代码

```typescript
// packages/xxx/src/yyy.ts:L1-L20
{pi 的关键代码片段}
```

### 2.3 pi 的局限

- {pi 不支持什么}
- {pi 的已知问题}

---

## 三、ion 已有能力

| 能力 | ion 实现位置 | 状态 |
|------|-------------|------|
| {能力 1} | [src/xxx.rs:L1-L10](file:///Users/xuyingzhou/Project/study-rust/ion/src/xxx.rs#L1-L10) | ✅ |
| {能力 2} | [src/xxx.rs:L20-L50](file:///Users/xuyingzhou/Project/study-rust/ion/src/xxx.rs#L20-L50) | ✅ |

---

## 四、差异点

| # | 维度 | pi | ion | 差距 |
|---|------|-----|------|------|
| 1 | {维度 1} | {pi 怎么做} | {ion 怎么做} | {差距描述} |
| 2 | {维度 2} | {pi} | {ion} | {差距} |

---

## 五、对齐方案

### P0（必须对齐 — 核心能力）

| # | 待对齐项 | 实现方案 | 验证 |
|---|---------|---------|------|
| P0.1 | {项 1} | {方案} | `{命令}` |
| P0.2 | {项 2} | {方案} | `{命令}` |

### P1（应该对齐 — 体验提升）

| # | 待对齐项 | 实现方案 |
|---|---------|---------|
| P1.1 | {项 1} | {方案} |

### P2（可选对齐 — 锦上添花）

| # | 待对齐项 | 说明 |
|---|---------|------|
| P2.1 | {项 1} | {为什么不紧急} |

---

## 六、实施进度

### 已完成

- ✅ {已完成项 1}（{日期}）
- ✅ {已完成项 2}

### 进行中

- 🔧 {进行中项}

### 待启动

- ⏳ {待启动项}

---

## 七、pi 不对齐的部分（ION 原创设计）

> pi 没有但 ION 需要的能力，记录原创设计思路。

| 能力 | ION 设计 | 原因 |
|------|---------|------|
| {能力 1} | {设计描述} | {为什么 pi 没有 / ION 为什么需要} |

参考：[TEAM_ORCHESTRATION.md](../design/TEAM_ORCHESTRATION.md)（多 Worker 团队编排，pi 没有）
