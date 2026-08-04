# 自主迭代 + MCP 闭环实验案例

> **状态：已验证** — 2026-07-31 完成，14 轮迭代 + bug 修复全程走通。

## 背景

验证 ion 的三个核心能力闭环：
1. **工具链协作**：`write`（画 SVG）+ `bash`（转 PNG）+ MCP `analyze_image`（视觉分析）三者无缝衔接
2. **自主迭代**：agent 读取 MCP 反馈 → 调整策略 → 重画 → 再分析，全程不需人介入
3. **A→B 自修复**：迭代过程中触发的 panic bug，走完整 A→B 流程修复（ZCode 不碰源码）

任务：让 ion 画蒙娜丽莎，调 MCP 视觉模型分析，递归优化多轮，目标是让 MCP 识别为"蒙娜丽莎"。

## 实验过程

### 阶段 1：单轮验证（v1）

```
用户 → ion "画蒙娜丽莎 SVG + 转 PNG + 调 MCP 分析"
         │
         ├─ write 工具：手写 SVG（金字塔构图/sfumato 渐变/微笑/头纱）
         ├─ bash 工具：rsvg-convert 转 PNG
         └─ MCP analyze_image（glm-4.6v 视觉模型）
              → 识别出深色长袍/头纱/交叉双手/对称构图 ✅
              → 但误判为《戴珍珠耳环的少女》❌
```

**关键发现**：MCP 对色彩/构图/背景元素识别准，但头纱被误读为"宗教兜帽"，身份识别错误。

### 阶段 2：递归 3 轮（v2→v4）

让 ion 基于 v1 的 MCP 反馈自主迭代 3 轮，每轮：复盘 → 画新版 → 转 PNG → MCP 分析 → 记录。

| 版本 | 重点改进 | MCP 识别结果 |
|------|---------|-------------|
| v2 | 强化微笑 + 眼神 | ✅ "Mona Lisa (La Gioconda)" 自信识别 |
| v3 | 柔软头纱 + 山水背景 | ⚠️ "referencing the Mona Lisa conceptually"（退化） |
| v4 | 暖金色调 + 构图加重 | ✅ "Mona Lisa"（靠姿势+头纱+构图） |

**v2 就达成了目标**——ion 自己记录了"胜利点"，但仍按 spec 跑满 3 轮。

### 阶段 3：深度递归 10 轮（v5→v14）

让 ion 跑满 10 轮，每轮聚焦不同特征（微笑/眼睛/背景/色彩/头发/手/光线/头纱/构图/终调）。

**关键设计**（写进 spec 防止偷懒）：
- 强制跑满 10 轮，不允许"满意就停"
- 每轮必须产出新 SVG + 调 MCP 分析（不许跳过）
- 退化也要诚实记录

### 阶段 4：发现 + 修复 panic bug

v5 跑时 ion exit 101 panic：
```
thread 'main' panicked at src/tool_loop_detector.rs:120:30:
end byte index 100 is not a char boundary; it is inside '名' (bytes 98..101)
```

**根因**：`compute_signature()` 的 `_` 分支用 `&args_str[..100]` 按字节切片，中文/emoji 多字节 UTF-8 跨界 panic。

**修复流程**（严格 A→B）：
1. ZCode 写 bug spec（`BUG-tool-loop-utf8.md`，含修复方案 + 3 个测试要求）—— 不碰 `src/`
2. `ion --host --agent coordinator` 启动
3. coordinator spawn developer（子 worker）
4. developer 在主仓库改 `src/tool_loop_detector.rs`：`&args_str[..100]` → `args_str.chars().take(100).collect()`
5. developer 加 3 个单元测试（纯 ASCII / 中文 / emoji 混合）
6. developer 自跑 `cargo test`（9 passed）+ `cargo build` + `cargo clippy`
7. commit `de04071`：`fix(tool-loop): UTF-8 截断 panic`

**三层独立验证**：
- ✅ `cargo test --lib tool_loop_detector` → 9 passed, 0 failed
- ✅ `cargo build` + `cargo clippy` 无新增 warning
- ✅ 端到端：用新二进制 + 长中文 prompt → ion 执行 write 工具写中文文件 → exit 0，不再 panic

> ⚠️ **踩坑教训**：A→B 流程里 developer 在 worktree 改源码 + commit 后，**主仓库的 `target/debug/ion` 二进制不会自动重新编译**。验证修复时必须先 `cargo build`，否则会误以为修复生效（短 prompt 不触发 bug 导致假阳性）。这次教训已记入。

### 阶段 5：14 版本客观评估

让 ion 用 MCP 重新逐张分析 14 个版本（每张用统一 prompt 打置信度 0-100），重建 `iteration-log.md`。

## 实验结果

### 置信度对比表（ion 用 MCP 客观评分）

| 版本 | 置信度 | 微笑 | 头纱 | 一句话 |
|:---:|:---:|:---:|:---:|------|
| **v1** | **90** ⭐ | ❌ | 部分 | 起点反而是最高分 |
| v2 | 70 | ❌ | ❌ | 第一次迭代反而退步 |
| v3 | 75 | ❌ | 部分 | |
| v4 | 65 | ❌ | ❌ | |
| v5 | **5** 💀 | ❌ | ❌ | 风格彻底跑偏 |
| v6 | **5** 💀 | ❌ | ❌ | 被判拒绝 |
| v7 | 60 | ❌ | ❌ | 加背景救回一些 |
| v8 | 75 | ❌ | 部分 | |
| v9 | 65 | 几乎 | ❌ | 头发画成辫子条纹 |
| v10 | **85** | ❌ | ❌ | 构图匹配强 |
| v11 | **85** | ❌ | ❌ | |
| v12 | **5** 💀 | 几乎 | ❌ | 又被判拒绝 |
| v13 | **85** | ❌ | ❌ | |
| v14 | **5** 💀 | ❌ | ❌ | 收尾反而崩了 |

完整数据见 [iteration-log.md](../../iteration-log.md)。

### 三个核心发现（ion 自己分析）

1. **置信度非单调，剧烈震荡**：在"可识别簇"（60-85）和"被拒绝簇"（5）之间反复跳。后期 v10-v14 没收敛反而 85↔5 交替——说明 LLM 改 SVG 时是**探索式漂移**，而非持续改进。

2. **最有效改进 TOP3**（按置信度跳跃）：
   - 🥇 v12→v13（+80 分）：恢复风景+正面肖像+交叠双手姿势
   - 🥈 v6→v7（+55 分）：加背景风景和清晰构图
   - 🥉 v1 本身（90 分）：起点就是最强模板

3. **最顽固问题——微笑**：14 个版本**没有一次**成功渲染出微笑。微笑依赖 sfumato 的脸颊/眼周柔光阴影，扁平 SVG 几何无法复现。

## 验证的能力闭环

| 能力 | 验证方式 | 结果 |
|------|---------|:---:|
| 工具链协作（write+bash+MCP） | 14 轮每轮都走完整链路 | ✅ |
| 自主迭代（读反馈→调策略） | ion 每轮据上轮反馈改不同特征 | ✅ |
| 真实 LLM 链路（非 mock） | MCP 走 glm-4.6v 视觉模型，每轮 40-50s | ✅ |
| 诚实自评 | ion 如实记录退化（v3/v5/v6/v12/v14） | ✅ |
| A→B 自修复 | panic bug 走完整 A→B，commit + 测试 + E2E 验证 | ✅ |
| 中文链路（修复后） | 用新二进制 + 中文 prompt 执行 write 工具 | ✅ |

## 产物清单

| 文件 | 说明 |
|------|------|
| `mona-lisa.svg` + `mona-lisa-v{2..14}.svg` | 14 个矢量版本（ion 用 write 画的） |
| `mona-lisa.png` + `mona-lisa-v{2..14}.png` | 14 个位图（ion 用 bash + rsvg-convert 转的） |
| `mona-lisa-evolution.png` | 14 版本演化拼图（带版本号+置信度标注，PIL 渲染） |
| `iteration-log.md` | ion 重建的完整评估报告（14 版本逐个 MCP 分析 + 对比表 + 趋势图） |
| `BUG-tool-loop-utf8.md` | panic bug 的 spec + 修复方案 + 测试要求 |
| commit `de04071` | `fix(tool-loop): UTF-8 截断 panic`（A→B 修复落地） |

## 教训与反思

### 教训 1：递归优化 ≠ 单调改进

让 LLM 自主迭代改 SVG，置信度不是单调上升而是**剧烈震荡**（90→5→85→5）。LLM 在改一个特征时可能把别的好的特征改坏。"递归 N 轮会越来越好"是错觉——每一轮都是独立探索，需要外部评估机制（如本实验的 MCP 打分）来挑选最优解，而非默认最后一轮最好。

### 教训 2：有些问题不是迭代能解的

微笑是矢量艺术的物理极限——它依赖 sfumato 的脸颊/眼周柔光阴影过渡，扁平 SVG 几何无法复现。14 轮迭代没解一次。**识别问题的物理边界，比盲目加迭代轮数更重要**。

### 教训 3：A→B 修复后必须重新编译

A→B 流程在 worktree 改源码 + commit 后，主仓库二进制不会自动更新。验证修复时必须先 `cargo build`，否则短 prompt 不触发 bug 会造成"修复生效"的假阳性。

### 教训 4：ion 的诚实自评有价值

ion 没有美化结果——它如实记录了 v3/v5/v6/v12/v14 的退化，最终报告直言"非单调震荡"和"微笑从未解决"。这种诚实对调试和决策很重要，是 agent 可靠性的体现。

## 相关文档

- [SELF_EVOLUTION.md](./SELF_EVOLUTION.md) — A→B 自进化架构总览
- [EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md) — 扩展系统（MCP 接入）
- [MCP_SYSTEM.md](./MCP_SYSTEM.md) — MCP 系统设计
