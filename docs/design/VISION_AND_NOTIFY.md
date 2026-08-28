# 视觉输入与异步委派通知

> **状态：已验证** — 端到端实测通过（vision_notify_ci.sh 12/12），随 2026-08-28 批次落地（commit 821ba5b / 075f8f6 / 09f6e4c）。

两个同属「worker 通信域」的能力：视觉输入打通"用户 → worker"的图片上行；异步委派完成通知打通"子 worker → 父 worker"的结果回传。文档合并记录共用的转发链路与各自的关键设计。

## 1. 视觉输入

### 1.1 数据流

```
UI（粘贴截图 / 回形针）
  → POST /api/prompt {session, text, behavior, images:[{data: base64, mimeType}]}
    → host socket 转发（cmd.session 路由）
      → worker_rpc "prompt"：解析 images（serde alias mimeType 兼容 camelCase）
        → agent.run_with_images(text, images)
          → user 消息 content = [Text, Image, Image...]（ContentBlock::Image）
            → transform_messages：model.input 含 "image" 则保留，否则替换占位符
              → provider 序列化（openai: image_url data URI；其余 provider 各自格式）
```

### 1.2 能力位判定（核心）

`transform_messages::downgrade_unsupported_images` 按 `model.input` 是否含 `"image"`
决定保留图片还是替换为 `(image omitted: ...)` 占位符。能力位来源链：

1. **权威源**：`~/.ion/models.json`，回退 `~/.pi/agent/models.json`
   （`registry.rs models_path()`，与 pi 一致）
2. ⚠️ `~/.ion/config.json` providers 的 input 字段**运行时被忽略**
   （注册表不从 config 读），仅 ion-console `/api/models` 徽标展示用——需与权威源手动对齐
3. ⚠️ 历史坑：解析器曾硬编码 `input: ["text"]`，模型定义里的能力位从不生效——
   "所有模型都是纯文本"、图片全被占位符替换（视觉从未通车的根因之一）

GLM 事实（pi models.json）：glm-4.7 / glm-5.3-flash = `[text, image]`；glm-5/5.1/5.2 = `[text]`。

### 1.3 set_model 运行时生效

`set_model` RPC 必须做三件事：改局部变量（响应用）、写 JSONL + SessionIndex（持久层）、
**`agent.set_model(model_reg.find_model(...))`（运行时）**。曾缺第三件——运行时 Model
永远停在启动值，切视觉模型后图片照样被占位。

### 1.4 UI 侧

- 粘贴（paste 事件 clipboardData.files）与回形针（file input）→ FileReader → base64
- 缩略图预览可删；切会话清空；入队失败恢复草稿
- 消息气泡渲染 Image 块（`flatten.ts` 提取，持久化后含缩略图）

## 2. 异步委派完成通知

### 2.1 问题

`spawn_worker(wait=false)` 异步委派后，子 worker 完成的结果无人回传——父 worker
不知道任务结束、拿不到输出。同步委派（wait=true）父已通过 await 拿到结果，无此问题。

### 2.2 设计

```
子 worker agent_end
  → host stdout 泵（两条都挂）
    → record.notify_parent?（注册时判定：config.parent 非空 && config.wait == Some(false)）
      → 读 record.latest_output 环形缓冲（末 3 条 delta 拼接，≤400 字符）
      → session id → worker id 解析（parent 字段存的是 _from_worker=session id！）
      → 父存活检查（Dead 跳过）
      → prompt {text: "【子任务完成】...", behavior: "followUp"} 注入父
        → 父忙：follow_up 队列，本轮结束自动消费开新轮
        → 父闲：立即唤醒开新一轮继续编排
```

- `ION_CHILD_NOTIFY=0` 关闭；`agent_stopped`（abort）不通知
- 同步/异步判别 = bridge 的 `wait` 字段（serde 直通 WorkerCreateConfig）

### 2.3 三个关键坑（复现过才写进来）

1. **数据源**：不能用 `get_last_assistant_text` RPC——spawn 子 worker 的会话文件
   不持久化对话（空 JSONL），且 agent_end 瞬间发 RPC 读内存有竞态（实测读到空）。
   泵里持有锁时读 latest_output 最可靠。
2. **parent 键类型**：`config.parent` 存的是调用者 session id（`_from_worker`），
   workers 表按 worker id 索引——不解析就查错键，误判"父已死"静默跳过（实测发生）。
3. **stdout 泵有两条**（worker_registry.rs ~1088 与 ~1730，事件按 worker 走不同条），
   改事件逻辑两条都要改（第一次只在泵 1 实现，实测通知不触发）。

## 3. Follow-up 队列（与通知共用通道）

运行中 prompt 按 behavior 分派（select 臂，2026-08-27 修）：

| behavior | 行为 |
|----------|------|
| `followUp`（默认） | 写 follow_up_tx 通道 → outer_loop 每轮结束 drain → follow_up_queue → 自动开新 turn |
| `steer` | 同通道，DeliverAs::Steer → steering 队列（LLM 在下一工具间隙看到） |
| `interrupt` | stopped + pause 句柄打断 |

⚠️ 历史坑：select 臂曾不识别 `prompt`（落 busy 兜底）；`follow_up` RPC 曾缓存到
`pending_steer_queue` 只在 run 结束后 drain——注入已退出的 run，消息黑洞
（UI 显示排队成功但永不送达，2026-08-27 修 075f8f6）。

## 4. 验证

| 层 | 手段 |
|----|------|
| 命令行 CI | `tests/vision_notify_ci.sh`（12 项：视觉判别/通知回灌/followUp 消费开新轮） |
| Harness | cargo test --lib 984/0（run_with_images 路径覆盖） |
| UI 自动化 | ion-console tests/ui.test.mjs T17（双按钮/排队条）、T18（视觉端到端，UI_PENDING=1）、T19（搜索无关） |
| 真实 E2E | 三级异步委派链（顶层→中层→叶子）逐级通知落地；2951 条消息会话不受影响 |
