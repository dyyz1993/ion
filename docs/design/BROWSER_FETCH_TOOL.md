# Browser Fetch Tool — SPA/CSR 感知网页抓取

> **状态：已验证** — fetch 工具已实现并通过三层验证（lib 单测 8 项 + FauxProvider harness 2 项 + `tests/browser_fetch_ci.sh` 12/12），真实外网 case（example.com）通过 `ION_E2E=1` 验证。

## 1. 定位

对接自研 Rust 浏览器引擎（`~/Project/study-rust/browser`，独立仓库）的 `browser fetch` CLI，为 agent 提供 **JS 动态渲染页面（SPA/CSR）** 的抓取能力。

**职责边界（用户定稿）**：本工具只管 SPA/CSR；静态页/SSR/纯 API 调用由 bash + curl 负责，工具描述里显式引导 LLM 分流。

## 2. 集成铁律（2026-09-10 与 browser 侧 M82/M83 对齐定稿）

| 铁律 | 实现 |
|------|------|
| **进程边界** | 内核直接 `tokio::process::Command` spawn 二进制，不走 bash、不产生 shell；永远不把 browser 合并进 ION workspace |
| **超时双保险** | 上游引擎自带全局预算（`--timeout-ms` 对所有 wait 策略生效，默认 60s）；ION 侧再套硬超时 `timeout_ms + 15s`，到点 kill——**不信任外部二进制不挂** |
| **warnings 透传** | `--json` 的 `warnings` 数组 + stderr `[warn]` 行（剥前缀去重）合并进响应，LLM 自判壳页换路径 |
| **max_length 截断** | 按字符（UTF-8 安全）截断到 50K（可配），`truncated: true` 标记 |

## 3. 工具规格

- **工具名**：`fetch`（浏览器引擎是实现细节，不进工具名）
- **参数**：`url`（必填，仅 http/https）、`format`（markdown 默认/html/text/links/images）、`wait_strategy`（load 默认/dom-ready/timeout）、`timeout_ms`（默认 75000）、`selector`（CSS 精抽）、`max_length`（默认 50000）
- **响应**（JSON 字符串）：
  ```json
  {"url":"…","title":"…","format":"markdown","content":"…","elapsed_ms":337,"truncated":false,"warnings":[]}
  ```
- **失败**：`AgentError::Tool`，含 stderr 尾部；缺二进制时含安装指引 `cargo install --git https://github.com/dyyz1993/browser browser-cli`

## 4. 配置（`~/.ion/config.json` 的 `fetch` 段）

```json
{
  "fetch": {
    "path": "/path/to/browser",
    "default_timeout_ms": 75000,
    "max_length": 50000,
    "allow_urls": ["https://*.example.com/*"]
  }
}
```

- **二进制查找链**：`ION_BROWSER_PATH` env（测试/CI 覆盖用）> `fetch.path` > PATH 里的 `browser` > `~/.ion/bin/browser`
- **URL 白名单**：`*`/`?` 通配匹配完整 URL；空数组 = 允许全部；整体开关用 Agent 工具黑/白名单

## 5. CLI 验证

### Group A: 基础抓取

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"fetch","args":{"url":"https://react.dev/learn","format":"text"}}'
```
验证点：XHR/JS 渲染内容出现在 `data.output`；`elapsed_ms`/`warnings`/`title` 字段齐全。

### Group B: 壳页警告

对反爬壳页调用 → `warnings` 非空（上游关键词启发 + "content is very short" 短内容启发）。

### Group C: URL 白名单

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"fetch","args":{"url":"https://other.com/"}}'
# → error 含 "not allowed by fetch.allow_urls"
```

### Group D: 缺二进制

`fetch.path` 指向不存在路径 → error 含 `cargo install --git`。

### 一键验证

```bash
bash tests/browser_fetch_ci.sh          # 12 项，全本地 fixture，不访问外网
```

## 6. 测试矩阵

| 层 | 位置 | 数量 | 说明 |
|----|------|------|------|
| lib 单测 | `src/browser_fetch.rs` | 8 | 通配匹配/白名单/内容提取/截断/scheme 拒绝/缺参/缺二进制 |
| harness | `tests/browser_fetch_harness.rs` | 2 | FauxProvider 驱动真实 agent loop：H1 工具调用闭环（本地 XHR fixture）、H2 白名单拒绝进对话 |
| 真实 case | 同上 `e2e_real_example_com` | 1 | `#[ignore]` + `ION_E2E=1`，真实外网 |
| CLI | `tests/browser_fetch_ci.sh` | 12 | 起 host + call_tool 全链路（Group A-D） |

## 7. 已知边界（与 browser 侧共识，2026-09-10）

- **CSR 覆盖率 KPI 只测无门卫纯 CSR 站**（react.dev 类为刻度，基线 17KB 全渲染）；juejin 类"风控对抗"站（bdms 签名门卫）单列，不记入管线能力分——这类站"不挂死 + warnings 明确"即为正确行为
- 上游引擎对强反爬（指纹/验证码）按设计不处理
- `network` 数组仅记 JS 发起的 fetch/XHR（M83 起 XHR 路径已通，有集成测试固化）
