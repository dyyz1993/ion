# 歌词改编工坊 · Web UI

网页版的歌词改编系统，跑在 ion serve 之上。改歌词 + 自检押韵（中文十三辙）/音节 + 审查员终审，全程实时流式。

## 快速开始

```bash
# 一键启动（自动起 ion serve + 装依赖 + 起网关）
bash webui/start.sh
# → 浏览器打开 http://localhost:8787
```

然后在网页里：
1. 左侧「原歌词」框粘贴一首歌的词
2. 「改编主题」填一个方向（或点预设按钮：程序员日常 / 赛博朋克 / …）
3. 点 **✍️ 开始改编**
4. 右侧实时看：生成流 → 工具调用 → 逐句对照表（违规红高亮）→ 审查员 VERDICT

## 架构

```
浏览器 ──HTTP/WS──> Node 网关 ──Unix socket──> ion serve ──> lyricist/critic agent
```

- **网关** (`gateway.mjs`)：把浏览器的 HTTP/WebSocket 透传到 ion 的 Unix socket，不理解业务。
- **agent** (`examples/agents/lyricist.md` + `critic.md`)：改编师 + 审查员，纯 Markdown 定义，零代码。
- **前端** (`index.html`)：单文件原生 JS，零构建。

详见 [docs/design/LYRIC_SYSTEM.md](../docs/design/LYRIC_SYSTEM.md)。

## 手动启动（不用 start.sh）

```bash
# 1. 确保 ion serve 在跑
ion serve &

# 2. 装网关依赖
cd webui && npm install

# 3. 起网关
node gateway.mjs --port 8787
```

## 端口 / 配置

| 参数 | 默认 | 说明 |
|------|------|------|
| `--port` | 8787 | 网关 HTTP/WS 端口 |
| `ION_BIN` 环境变量 | `ion` | ion 二进制路径（start.sh 用） |

## 故障排查

| 现象 | 原因 / 解决 |
|------|------------|
| 网页打开但 `/rpc` 返回 502 | `ion serve` 没跑。`start.sh` 会自动起；手动就 `ion serve &` |
| `create_session` 报 agent not found | lyricist/critic 没加载。确认 `examples/agents/lyricist.md` 存在，或用 `--agent build` 兜底 |
| 改编很久没结果 | 真实 LLM 调用慢；展开「原始事件流」看是否还在推 `text_delta` |
| agent 输出跑偏（不改编、聊别的） | 多半是旧 session 污染。清掉重来：`rm -rf ~/.ion/agent/sessions/ ~/.ion/agent/last_session` 后重启 host |
| `Host already running` 起不来新 host | watchdog 在抢占 socket：`pkill -f "scripts/watchdog"` 后重试 |
| 押韵检测不准 | Phase 1 靠 LLM 自检；确定性硬编码检测在 Phase 2（见设计文档 §5） |

## 测试

```bash
# 默认：网关协议层（12 项，不调真 LLM）
bash tests/lyric_webui_ci.sh

# E2E：完整链路含真实 glm-5.2 改编 + 审查（19 项，有 LLM 成本）
ION_E2E=1 bash tests/lyric_webui_ci.sh
```

E2E 覆盖：WebSocket 流式透传（text_delta）+ lyricist 产出 `<lyric_result>` + critic 产出 VERDICT。

## 文件

| 文件 | 作用 |
|------|------|
| `gateway.mjs` | Unix socket ↔ HTTP/WS 桥（依赖 `ws`） |
| `index.html` | 单文件前端（输入 / 实时流 / 对照表 / VERDICT） |
| `start.sh` | 一键启动 |
| `package.json` | 声明 `ws` 依赖 |
