# ZCode Hook：防止误杀 ion 进程

> **状态：已验证** — hook 已部署，三层防御通过 20+ 用例测试。
>
> ⚠️ 本文讲的是 **zcode 编辑器/CLI 的 PreToolUse hook**（在 zcode 里开发 ion 时，防止 AI 误杀系统进程），**不是** ION 项目内部的 HOOK_SYSTEM（那是 ion 自己给用户提供的 hook 系统，见 [HOOKS_AND_OUTLINE_SYNC.md](../design/HOOKS_AND_OUTLINE_SYNC.md)）。两个"hook"名字撞了，但完全是两回事。

---

## 为什么需要这个 hook

用 zcode（或任何 AI 编码 agent）开发 ion 时，AI 经常需要清理测试残留的 ion 进程。它会本能地写：

```bash
pkill -f ion          # ❌ 极其危险
pkill -f "ion serve"  # ❌ 危险
```

问题是：**系统里含 "ion" 的进程，绝大多数不是我们的 ion。** 实测一台 mac 上：

| 进程 | 真实身份 | `pkill -f ion` 会怎样 |
|------|---------|----------------------|
| `logioptionsplus_agent` | **罗技鼠标键盘驱动**（名字含 "ion"）| 🔴 被杀 → 鼠标键盘失灵 |
| `logioptionsplus_updater` | 罗技驱动更新服务 | 🔴 被杀 |
| `fseventsd` / `locationd` / `maild` / `cfprefsd` | **macOS 系统服务** | 🔴 被杀 → 系统异常 |
| `suhelperd` | macOS 软件更新 | 🔴 被杀 |
| `ZCode Helper` / `ChatGPT` / `Chrome Helper` | 用户 App | 🔴 被杀 → 应用崩溃 |

光 `ps aux | grep ion` 能列出一百多个进程，**全是误伤目标**。

这个 hook 的作用：在 AI 执行 kill/pkill 命令**之前**拦截，检查目标是不是系统进程，是就拦下来并把正确做法告诉 AI。

---

## 怎么部署的

两个文件，都在项目级 `.zcode/` 下（所以只在 ion 项目里生效）：

```
.zcode/
├── config.json                      ← 注册 hook 到 PreToolUse 事件
└── hooks/
    └── guard-ion-kill.sh            ← 拦截逻辑（bash + python3）
```

### `.zcode/config.json`

```json
{
  "hooks": {
    "enabled": true,
    "events": {
      "PreToolUse": [
        {
          "matcher": "Bash",
          "hooks": [
            {
              "type": "command",
              "command": "bash \"${ZCODE_PROJECT_DIR}/.zcode/hooks/guard-ion-kill.sh\"",
              "timeout": 5,
              "statusMessage": "检查 kill/pkill 安全性"
            }
          ]
        }
      ]
    }
  }
}
```

要点：
- `matcher: "Bash"` — 只对 Bash 工具触发（大小写敏感，必须是 `Bash`）
- `type: "command"` — zcode hook 只支持 `command`（shell 脚本）和 `process`（可执行文件），**没有 agent 类型**
- `timeout: 5` — 5 秒超时（`command` 类型的单位是**秒**）
- `${ZCODE_PROJECT_DIR}` — zcode 注入的项目根路径变量

---

## 三层防御逻辑

hook 脚本（`guard-ion-kill.sh`）按顺序检查：

### 第一层：逃生通道（放行）

命令里带 `# ion-safe` 注释 → 直接放行。

```bash
kill 1292  # ion-safe    ← AI/人确认过要杀这个，放行
```

**为什么需要逃生通道**：zcode hook 不支持 `agent` 类型（不能在 hook 里起一个 LLM 会话判断），所以"不确定时交给 agent 识别"通过这个显式注释实现——AI 想清楚要杀谁，加注释显式确认，hook 就放行。

### 第二层：命令格式拦截（pkill/killall 通配）

```
pkill -f ion              → ❌ 拦截（裸 "ion" 通配，看不到目标）
pkill -f "ion serve"      → ❌ 拦截（无路径前缀）
pkill -f ion_sse_proxy    → ❌ 拦截（ion 前缀通配）
killall ion               → ❌ 拦截
```

例外——**完整路径白名单**（含这些标记就放行）：
```bash
pkill -f "target/debug/ion serve"   ✅ 放行
pkill -f "study-rust/ion"           ✅ 放行
```

### 第三层：PID 目标查询（kill <数字PID>）

拿到 `kill <PID>` 后，用 `ps -p <PID> -o command=` 查这个 PID 到底是什么，按路径分类：

| PID 的进程路径 | 分类 | 决策 |
|---------------|------|------|
| `/System/` `/usr/lib` `/sbin/` `/private/var/db/` | macOS 系统 | ❌ 拦截 |
| `/Library/` | 系统驱动/服务（罗技等）| ❌ 拦截 |
| `/Applications/` | 用户 App（ZCode/Chrome 等）| ❌ 拦截 |
| `target/debug/ion` / `target/release/ion` / `study-rust/ion` | **★ 我们的 ion** | ✅ 放行 |
| PID 不存在 / 已退出 | — | ✅ 放行（kill 个空无害）|
| home 下的其他用户进程 | 其他 | ✅ 放行 |

**这一层的价值**：光看命令格式发现不了的问题，查 PID 才知道。例如 `kill 123 456 789`，前两个不存在，但 789 是 `suhelperd`（macOS 软件更新服务）——只有查 PID 才能拦住。

---

## 正确做法（hook 拦截后会提示 AI 这些）

要杀 ion 进程，四种正确姿势：

```bash
# 1. 先查 PID，再精确 kill（最安全）
pgrep -f 'target/debug/ion serve'
kill <PID>

# 2. 按 socket 反查 PID（ion serve 绑定 ~/.ion/host.sock）
lsof -ti ~/.ion/host.sock | xargs kill

# 3. 完整路径 pkill（白名单）
pkill -f "target/debug/ion serve"

# 4. 如果真要通配（极少数情况），加注释显式确认
pkill -f ion  # ion-safe
```

---

## 验证 hook 是否生效

### 方法 1：直接跑脚本

```bash
HOOK=".zcode/hooks/guard-ion-kill.sh"

# 应拦截（系统进程）
echo '{"tool_name":"Bash","tool_input":{"command":"kill 1292"}}' | bash "$HOOK"
# → {"decision":"deny","reason":"禁止 kill 系统级服务/驱动 (PID 1292): /Library/.../logioptionsplus_agent..."}

# 应拦截（裸通配）
echo '{"tool_name":"Bash","tool_input":{"command":"pkill -f ion"}}' | bash "$HOOK"
# → {"decision":"deny","reason":"禁止 pkill/killall 通配符杀进程..."}

# 应放行（正常命令，空输出 + exit 0）
echo '{"tool_name":"Bash","tool_input":{"command":"cargo build"}}' | bash "$HOOK"
```

### 方法 2：在 zcode 会话里观察

⚠️ **hook 在会话启动时加载**。如果你改了 `.zcode/config.json`，当前已开的会话**不会热加载**——要么新开会话，要么在设置里 reload。

新会话生效后，AI 尝试危险 kill 时会收到 deny 响应，AI 会看到拦截原因和正确做法，然后改用安全的命令。

---

## 常见问题

**Q: hook 配置改了不生效？**
A: zcode 的 hook 在**会话启动时加载**，当前会话不会热加载。新开一个会话即可。

**Q: AI 真的需要杀进程，但被拦了怎么办？**
A: 三种情况：
1. AI 杀错了目标（杀到系统进程）→ hook 拦得对，AI 应该改用精确 PID
2. AI 用了通配但目标确实是我们的 ion → 用完整路径 `pkill -f "target/debug/ion serve"`
3. AI 确认要通配 → 加注释 `# ion-safe`

**Q: 为什么不用 zcode 的 `agent` 类型 hook 交给 LLM 判断？**
A: zcode hook **只有 `command` 和 `process` 两种类型**，没有 `agent` 类型。在 hook 里调 LLM 会阻塞工具执行、烧 token、还可能超时。所以用"查 PID（确定性判断）+ `# ion-safe` 注释（AI 显式确认）"替代。

**Q: 这个 hook 只保护 ion 项目吗？**
A: 是。配置在项目级 `.zcode/config.json`，只在 `/Users/xuyingzhou/Project/study-rust/ion` 目录下生效。其他项目不受影响。如果想在所有项目都启用，把同样的 `hooks` 配置加到 `~/.zcode/cli/config.json` 的顶层 `hooks` key 下。

**Q: 为什么不直接拦所有 pkill/killall？**
A: 因为 AI 有合理需求（比如 `pkill -f "target/debug/ion serve"` 是安全的）。一刀切会卡住正常操作。白名单 + PID 查询的组合既能防误杀，又不影响正常使用。

---

## 文件清单

| 文件 | 作用 |
|------|------|
| [`.zcode/config.json`](../../.zcode/config.json) | 注册 hook 到 PreToolUse/Bash |
| [`.zcode/hooks/guard-ion-kill.sh`](../../.zcode/hooks/guard-ion-kill.sh) | 三层防御拦截脚本（bash + python3）|
