# Dev Server Detector 设计文档

> **状态：已完成** — A→B 流程实现（commit `270a392`），646 行 + 12 单元测试全过，全量 984 lib tests 无回归，CI 脚本 `tests/dev_server_detector_ci.sh` 实测通过。

---

## 概览

当 Agent 在 bash 工具里启动了 dev server（`npm run dev` / `vite` / `next dev` / `python -m http.server` 等），内核自动检测输出里的端口（或后台探活兜底），并在**每次 LLM 请求前**往 system prompt 追加一段 `<dev_servers>` XML，告知 LLM 当前有哪些 dev server 在跑、端口多少、存活多久。LLM 据此自行决定是否用 `bash curl localhost:PORT` 或浏览器工具排查运行时状态。

**一句话**：让 LLM "感知到"它自己（或用户）刚起了一个 web server，而不是对后台进程一无所知。

### 解决什么问题

| 问题 | 没有本扩展 | 有本扩展 |
|------|-----------|---------|
| Agent 跑 `npm run dev &` 后，LLM 不知道 server 起来了 | ✅ 盲区：LLM 可能继续写代码，意识不到有 server 可测 | ✅ 下一轮 prompt 自动带 `<dev_servers port="3000">` |
| Agent 想验证前端改动，但不知道端口 | 要 LLM 自己 grep stdout 或猜端口 | 直接从 system prompt 读到端口 |
| 后台启动的 server（stdout 还没打印端口） | 漏检 | 后台探活兜底（15s 内探测常见端口） |

### 不做什么（明确边界）

- ❌ **不开浏览器**（与 BrowserDevtoolsExtension 区分，那是后续工作）
- ❌ **不抓 console error**（不做实时 push，避免 context 污染）
- ❌ **不 spawn worker / 不触发动作**（只读、只注入，不主动改世界）
- ❌ **不做定时轮询**（event 驱动：bash 跑完那一刻检测一次 + 探活兜底，不需要 interval loop）

> 这是 MonitorExtension 的反面：Monitor 是 **poll 模型**（定时跑外部脚本 → spawn worker），本扩展是 **event 模型**（bash 输出 → 改当前 session 的 system prompt）。两者架构正交，互不替代。

### 对齐 pi

pi 无此能力（pi 的 bash 是无状态的，不感知后台进程）。这是 ION 原创设计，借鉴 LSP Extension 的"工具执行后异步检测 + 下一轮注入"模式。

### 能力清单

| 能力 | 入口 | 状态 |
|------|------|------|
| 检测 bash stdout 里的 dev server 端口 | `on_tool_execution_end`（手写扫描） | ✅ |
| 后台端口探活兜底（后台启动漏检补救） | `tokio::spawn` + TcpStream::connect | ✅ |
| system prompt 注入 `<dev_servers>` XML | `on_system_prompt(&mut String)` | ✅ |
| session 级状态隔离（worker 边界） | `Vec<DetectedServer>` on self | ✅ |
| 去重（相同端口集合不重复追加） | signature 字符串比对（抄 LSP） | ✅ |
| CLI 查询当前检测到的端口 | `extension_rpc: list/clear/probe` | ✅ |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | 扫 bash stdout 提取端口（3 种模式） | ✅ | `cargo test --lib dev_server_detector`（12 passed）|
| 1.2 | 端口探活兜底（8 常见端口扫描） | ✅ | `extension_rpc probe` 返回 probed_ports + alive |
| 2.1 | on_system_prompt 注入 `<dev_servers>` | ✅ | signature 变化时追加 XML |
| 2.2 | 去重（signature 比对） | ✅ | signature 不变则不重复追加 |
| 3.1 | session 隔离（worker 级） | ✅ | 新 session list 返回 0（CI C1） |
| 4.1 | extension_rpc: list/clear/probe | ✅ | `tests/dev_server_detector_ci.sh` 全过 |
| 5.1 | framework 推断 | ✅ | `infer_framework()` 从 source_cmd 推断 next/vite/flask |

---

## 1. 配置

**文件**：`src/dev_server_detector.rs`（新建）+ `src/config.rs`（加字段）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevServerDetectorConfig {
    /// 是否启用（默认 false，对齐 file-snapshot 默认关闭的策略）
    pub enabled: bool,
    /// 探活超时（秒），默认 15
    pub probe_timeout_secs: u64,
    /// 探活端口清单（兜底用，覆盖主流框架默认端口）
    pub probe_ports: Vec<u16>,
}
```

默认值：
```rust
impl Default for DevServerDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_timeout_secs: 15,
            probe_ports: vec![3000, 5173, 8080, 8000, 4200, 8888, 4173, 5000],
        }
    }
}
```

config.json 示例：
```json
{
  "extensions": {
    "dev_server_detector": {
      "enabled": true,
      "probe_timeout_secs": 15,
      "probe_ports": [3000, 5173, 8080, 8000, 4200]
    }
  }
}
```

**端口清单说明**（覆盖主流框架默认端口）：

| 端口 | 框架/工具 |
|------|----------|
| 3000 | Next.js / React (create-react-app) / Node Express |
| 5173 | Vite |
| 8080 | Webpack Dev Server / Spring Boot / Go net/http 常用 |
| 8000 | Django / Python http.server / FastAPI (uvicorn 默认) |
| 4200 | Angular |
| 8888 | Jupyter Notebook |
| 4173 | Vite preview |
| 5000 | Flask |

---

## 2. 主流程 / 数据结构

**文件**：`src/dev_server_detector.rs`（新建）

### 2.1 核心 Struct

> **架构关键点**：ION 每个 worker 拥有独立的 `ExtensionManager`（见 `src/agent/agent_loop.rs:507` 等处 `self.extensions` 是 worker 级字段）。因此 worker 级扩展的 `self` 天然绑定当前 worker/session，**不需要从 `on_system_prompt` 参数拿 session_id，也不需要 `HashMap<session_id, ...>` 做隔离**——worker 边界即 session 边界。参照 `src/agent/memory.rs:665` 的 `on_system_prompt` 实现，直接用 `self.store`（worker 初始化时绑好）。

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use std::time::Instant;

/// 单个被检测到的 dev server
#[derive(Debug, Clone, Serialize)]
pub struct DetectedServer {
    pub port: u16,
    /// 触发检测的原始命令（bash 工具的 command 参数，截断到 200 字符）
    pub source_cmd: String,
    /// 检测方式："stdout_regex" | "probe"
    pub detected_via: String,
    /// 首次检测到的时间戳
    pub first_seen: Instant,
    /// 端口是否存活（探活确认）
    pub alive: bool,
}

/// 扩展主结构（非 singleton，每个 worker 一个实例）
/// state 直接挂在 self 上，worker 边界天然隔离 session
pub struct DevServerDetectorExtension {
    /// 当前 worker/session 检测到的 dev server 列表
    servers: Arc<Mutex<Vec<DetectedServer>>>,
    /// 上次注入 system prompt 用的 signature（端口集合的规范化字符串）
    /// 用于去重：signature 不变则不重复追加
    last_injected_signature: Arc<Mutex<Option<String>>>,
    config: DevServerDetectorConfig,
}
```

### 2.2 钩子一：`on_tool_execution_end`（检测入口）

```rust
async fn on_tool_execution_end(
    &self,
    ctx: &ToolExecutionContext,
) -> AgentResult<()> {
    // Step 1: 只关心 bash 工具
    if ctx.tool_name != "bash" {
        return Ok(());
    }

    // Step 2: 扫 stdout，提取端口（见 §2.4 正则模式表，手写扫描不用 regex crate）
    let stdout = &ctx.result;
    let ports_from_stdout = extract_ports_from_stdout(stdout);

    // Step 3: 命中端口 → 记录
    {
        let mut servers = self.servers.lock().await;
        for port in &ports_from_stdout {
            add_server_if_new(&mut servers, *port, &ctx.tool_input_cmd, "stdout_regex");
        }
    }  // ⚠️ 显式 drop lock，不持锁跨 await（抄 monitor 死锁修复教训）

    // Step 4: 若 stdout 没检测到 + 命令疑似 dev server → 后台探活兜底
    //   （npm run dev & 这种后台启动，stdout 可能还没打印端口）
    let looks_like_dev_server = is_dev_server_command(&ctx.tool_input_cmd);
    if ports_from_stdout.is_empty() && looks_like_dev_server {
        let servers = self.servers.clone();
        let config = self.config.clone();
        let cmd = ctx.tool_input_cmd.clone();
        tokio::spawn(async move {
            probe_and_record(servers, cmd, config).await;
        });
    }

    Ok(())
}
```

### 2.3 钩子二：`on_system_prompt`（注入入口）

> 注意：`on_system_prompt(&self, prompt: &mut String)` 签名里没有 context，但**不需要 session_id**——因为 `self` 是 worker 级实例，天然绑定当前 session（见 §2.1 架构关键点）。参照 `src/agent/memory.rs:665` 的 Memory 扩展同款写法。

```rust
async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
    let mut to_inject: Option<String> = None;
    {
        let mut servers = self.servers.lock().await;

        // Step 1: 过滤掉已死的 server（探活失败超过 30s 的清掉）
        servers.retain(|s| s.alive || s.first_seen.elapsed() < Duration::from_secs(30));

        if servers.is_empty() {
            return Ok(());  // 没检测到任何 server，不注入
        }

        // Step 2: 计算 signature（端口集合 + 存活状态的规范化字符串）
        let signature = compute_signature(&servers);

        // Step 3: 去重 —— signature 没变就不重复追加
        let mut last_sig = self.last_injected_signature.lock().await;
        if last_sig.as_deref() == Some(&signature) {
            return Ok(());  // 已经注入过同样的内容
        }

        // Step 4: 拼 XML
        to_inject = Some(format_dev_servers_xml(&servers));
        *last_sig = Some(signature);
    }  // drop locks

    if let Some(xml) = to_inject {
        prompt.push_str("\n\n");
        prompt.push_str(&xml);
    }

    Ok(())
}
```

### 2.4 正则模式表（端口提取）

**文件**：`src/dev_server_detector.rs` 的 `extract_ports_from_stdout()`

| 框架/工具 | stdout 典型输出 | 匹配正则 | 提取端口 |
|----------|----------------|---------|---------|
| **Vite** | `➜ Local: http://localhost:5173/` | `localhost:(\d+)` | 5173 |
| **Vite (alt)** | `➜ Network: http://192.168.1.5:5173/` | `:(\d{4,5})(?=/|$)` | 5173 |
| **Next.js** | `▲ Local: http://localhost:3000` | `localhost:(\d+)` | 3000 |
| **create-react-app** | `Local: http://localhost:3000` | `localhost:(\d+)` | 3000 |
| **webpack-dev-server** | `<i> [webpack-dev-server] Project is running at http://localhost:8080/` | `localhost:(\d+)` | 8080 |
| **Python http.server** | `Serving HTTP on 0.0.0.0 port 8000` | `port (\d+)` | 8000 |
| **Django** | `Starting development server at http://127.0.0.1:8000/` | `localhost:(\d+)` 或 `127.0.0.1:(\d+)` | 8000 |
| **Flask** | `* Running on http://127.0.0.1:5000` | `127\.0\.0\.1:(\d+)` | 5000 |
| **FastAPI/uvicorn** | `Uvicorn running on http://127.0.0.1:8000` | `127\.0\.0\.1:(\d+)` | 8000 |
| **Angular** | `** Angular Live Development Server is listening on localhost:4200` | `localhost:(\d+)` | 4200 |
| **Go (通用)** | `Listening on :8080` / `Server starting on port 8080` | `(?:listening|starting|running).*?(?::|port\s+)(\d{2,5})` | 8080 |
| **Express** | `Server running on port 3000` | `port (\d+)` | 3000 |
| **通用兜底** | 任何 `localhost:NNNN` / `127.0.0.1:NNNN` / `0.0.0.0:NNNN` | `(?:localhost|127\.0\.0\.1|0\.0\.0\.0):(\d{2,5})` | — |

**实现策略**（不用 `regex` crate，ION 全项目刻意避免 regex，见 explore agent 调研结论）：

```rust
/// 从 stdout 提取端口号（不依赖 regex crate，手写扫描）
fn extract_ports_from_stdout(stdout: &str) -> Vec<u16> {
    let mut ports = Vec::new();
    // 按行扫描
    for line in stdout.lines() {
        let line_lower = line.to_lowercase();
        // 模式 1: localhost:PORT / 127.0.0.1:PORT / 0.0.0.0:PORT
        for prefix in &["localhost:", "127.0.0.1:", "0.0.0.0:"] {
            if let Some(idx) = line_lower.find(prefix) {
                let after = &line[idx + prefix.len()..];
                if let Some(port) = parse_leading_digits(after) {
                    if (1024..=65535).contains(&port) {
                        ports.push(port);
                    }
                }
            }
        }
        // 模式 2: "port NNNN" / "Port NNNN" / "PORT NNNN"
        if let Some(idx) = line_lower.find("port ") {
            let after = &line[idx + 5..];
            if let Some(port) = parse_leading_digits(after) {
                if (1024..=65535).contains(&port) && !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }
        // 模式 3: "listening on :NNNN" / "on :NNNN"
        if let Some(idx) = line_lower.rfind(":") {
            let after = &line[idx + 1..];
            // 确认冒号前是 "on " 或 "listening" 之类，且后面是纯数字
            if let Some(port) = parse_leading_digits(after) {
                let before = line_lower[..idx].trim_end();
                if before.ends_with("on") || before.ends_with("listening") {
                    if (1024..=65535).contains(&port) && !ports.contains(&port) {
                        ports.push(port);
                    }
                }
            }
        }
    }
    ports
}

/// 解析字符串开头的连续数字（遇到非数字停止）
fn parse_leading_digits(s: &str) -> Option<u16> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
```

**dev server 命令识别**（决定是否触发后台探活）：

```rust
fn is_dev_server_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // 匹配常见 dev server 启动命令
    let patterns = [
        "npm run dev", "npm start", "yarn dev", "yarn start",
        "pnpm dev", "pnpm start", "bun dev", "bun run dev",
        "vite", "next dev", "nuxt dev", "ng serve",
        "webpack-dev-server", "parcel",
        "python -m http.server", "python3 -m http.server",
        "manage.py runserver",  // Django
        "flask run", "uvicorn ", "fastapi ",
        "go run main.go", "air",  // Go
        "php artisan serve",
        "rails server", "rails s",
        "docker compose up", "docker-compose up",
    ];
    patterns.iter().any(|p| lower.contains(p))
}
```

### 2.5 后台端口探活（兜底）

```rust
/// 后台探活：并发连接 probe_ports 清单，存活的记入 servers
async fn probe_and_record(
    servers: Arc<Mutex<Vec<DetectedServer>>>,
    source_cmd: String,
    config: DevServerDetectorConfig,
) {
    use tokio::net::TcpStream;
    use std::time::Duration;

    let timeout = Duration::from_secs(2);  // 单端口连接超时
    let mut alive_ports: Vec<u16> = Vec::new();

    // 并发探测所有候选端口
    let probes: Vec<_> = config.probe_ports.iter().map(|&port| {
        let port = port as u16;
        tokio::spawn(async move {
            let addr = format!("127.0.0.1:{}", port);
            match tokio::time::timeout(
                timeout,
                TcpStream::connect(&addr)
            ).await {
                Ok(Ok(_)) => Some(port),
                _ => None,
            }
        })
    }).collect();

    // 等待全部探活完成（最多 probe_timeout_secs，由外层 timeout 兜底）
    for handle in probes {
        if let Ok(Some(port)) = handle.await {
            alive_ports.push(port);
        }
    }

    // 存活端口记入 servers
    if !alive_ports.is_empty() {
        let mut servers = servers.lock().await;
        for port in alive_ports {
            add_server_if_new(&mut servers, port, &source_cmd, "probe");
        }
    }
}
```

### 关键决策点

| 场景 | 处理 |
|------|------|
| bash 跑的是 `ls` / `cat`（非 server 命令） | `on_tool_execution_end` 直接 return，不做任何处理 |
| bash stdout 明确含 `localhost:3000` | stdout_regex 提取端口，立即记录，不触发探活（已检测到） |
| bash 跑 `npm run dev &`（后台，stdout 无端口） | 命令匹配 dev server 模式 + stdout 无端口 → spawn 后台探活 |
| 探活时端口还没 listen（server 启动慢） | 探活失败，不记录；但 30s 内 server 打印端口到后续 bash 输出时仍会被捕获 |
| 多个 worker 各自跑了 dev server | worker 级实例天然隔离（每个 worker 独立 `ExtensionManager` + 独立 `self`），互不串 |
| 同一 session 重复跑 `npm run dev`（端口相同） | `add_server_if_new` 去重，不重复记录 |
| 端口已死（server 被 kill） | `on_system_prompt` 里 `retain(alive)` 过滤；探活失败的 30s 后清除 |

---

## 3. 注入 XML schema

**文件**：`src/dev_server_detector.rs` 的 `format_dev_servers_xml()`

### 3.1 格式

```xml
<dev_servers count="1">
<server port="3000" framework="next" cmd="npm run dev" age="2m" via="stdout_regex"/>
<server port="5173" framework="vite" cmd="vite" age="1m" via="probe"/>
</dev_servers>
```

**字段说明**：

| 属性 | 类型 | 说明 |
|------|------|------|
| `count` | number | server 总数（顶层属性） |
| `port` | number | 端口号 |
| `framework` | string | 推断的框架（next/vite/flask/unknown），从 source_cmd 推断 |
| `cmd` | string | 触发命令（截断到 80 字符，避免 prompt 膨胀） |
| `age` | string | 存活时长（人类可读：`30s` / `2m` / `1h`） |
| `via` | string | 检测方式：`stdout_regex`（stdout 命中）或 `probe`（探活兜底） |

### 3.2 去重机制（抄 LSP signature 比对）

**问题**：`on_system_prompt` 每次 LLM 请求都调，若每次都追加 XML，prompt 会无限膨胀。

**解决**：计算 signature = 排序后的 `port:alive` 集合字符串。signature 没变就不重复追加。

```rust
fn compute_signature(servers: &[DetectedServer]) -> String {
    let mut sigs: Vec<String> = servers.iter()
        .map(|s| format!("{}:{}", s.port, s.alive))
        .collect();
    sigs.sort();
    sigs.join(",")
}
// 示例: "3000:true,5173:true"
```

**效果**：
- 首次检测到 port 3000 → signature `"3000:true"` → 追加 XML
- 下一轮 LLM 请求，signature 还是 `"3000:true"` → **不重复追加**
- server 挂了，port 3000 变 `false` → signature 变化 → 追加更新后的 XML
- 新增 port 5173 → signature 变化 → 追加

> 注意：ION 的 system prompt 是每轮重新构建的（不是增量追加），所以这里的"去重"是指"这一轮要不要往 prompt 里加 XML"，不是"防止 prompt 累积"。signature 机制确保只有端口集合**变化时**才加 XML，稳定状态下加一次即可（因为 prompt 每轮重建，加一次就够了）。

### 3.3 session 隔离（worker 边界即 session 边界）

**无需手动隔离**。ION 每个 worker 拥有独立的 `ExtensionManager`（`src/agent/agent_loop.rs:507` 等处 `self.extensions` 是 worker 级字段），本扩展作为非 singleton 注册时，每个 worker 各持有一个 `DevServerDetectorExtension` 实例，`self.servers` 天然按 worker 隔离。

- 场景 3（`ion serve`）多 session 并发：每个 worker 的 `self.servers` 独立，互不串
- worktree 场景：不同 worktree 是不同 worker/session，端口不串
- 不做跨 session 聚合（一个 worker 不该知道另一个 worker 的端口）

> 这跟 Memory / Plan / ContextIndex / Bash 扩展的隔离模型完全一致——它们的 `on_system_prompt` 里也是直接用 `self.store` / `self.xxx`，不传 session_id。

---

## 4. 接口规格

### 4.1 `extension_rpc: list` — 列出当前 session 检测到的 dev server

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"list","params":{}}'
```

**请求参数：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `extension` | string | 是 | 固定 `"dev_server_detector"` |
| `method` | string | 是 | 固定 `"list"` |
| `params` | object | 否 | 无参数 |

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "extension_rpc",
  "success": true,
  "data": {
    "ok": true,
    "result": {
      "servers": [
        {
          "port": 3000,
          "framework": "next",
          "source_cmd": "npm run dev",
          "detected_via": "stdout_regex",
          "age_secs": 120,
          "alive": true
        }
      ],
      "count": 1
    }
  }
}
```

**响应 JSON（无 server）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "extension_rpc",
  "success": true,
  "data": {"ok": true, "result": {"servers": [], "count": 0}}
}
```

### 4.2 `extension_rpc: clear` — 清除当前 session 的检测结果

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"clear","params":{}}'
```

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "extension_rpc",
  "success": true,
  "data": {"ok": true, "result": {"cleared": 1}}
}
```

### 4.3 `extension_rpc: probe` — 手动触发端口探活

**请求：**

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"probe","params":{}}'
```

**响应 JSON（成功）：**

```json
{
  "type": "response",
  "id": "1",
  "command": "extension_rpc",
  "success": true,
  "data": {
    "ok": true,
    "result": {
      "probed_ports": [3000, 5173, 8080, 8000, 4200],
      "alive": [3000],
      "newly_detected": 1
    }
  }
}
```

---

## 5. CLI 测试指南

> 详细测试 case 见 `tests/dev_server_detector_ci.sh`。

### Group A：stdout 正则检测（核心路径）

**前置**：`config.json` 里 `extensions.dev_server_detector.enabled = true`

#### A1 Vite 输出格式

```bash
# 起 host
ion serve &
sleep 2

# 创建 session
SID=$(ion rpc --method create_session --params '{"agent":"default"}' | jq -r '.data.session_id')

# 模拟 bash 工具执行（通过 call_tool RPC，让 extension 的 on_tool_execution_end 触发）
ion rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","input":{"command":"echo \"➜  Local:   http://localhost:5173/\""}}'

# 查询检测结果
ion rpc --session "$SID" --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"list","params":{}}'
```

**验证点：**
- ✅ `list` 返回 1 个 server，port=5173
- ✅ `detected_via = "stdout_regex"`
- ✅ `framework = "vite"`（从 echo 内容推断，或 unknown，二选一）

#### A2 Next.js 输出格式

```bash
ion rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","input":{"command":"echo \"▲ Local:   http://localhost:3000\""}}'
```

**验证点：**
- ✅ `list` 返回 2 个 server（5173 + 3000）

#### A3 Python http.server 输出

```bash
ion rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","input":{"command":"echo \"Serving HTTP on 0.0.0.0 port 8000\""}}'
```

**验证点：**
- ✅ port=8000 被检测到（模式 2: "port NNNN"）

#### A4 非 server 命令不触发

```bash
ion rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","input":{"command":"ls -la"}}'
```

**验证点：**
- ✅ server 数量不变（ls 不触发检测）

### Group B：system prompt 注入

#### B1 下一轮 prompt 含 XML

```bash
# A1 检测到 5173 后，发一条消息触发 LLM 请求
echo "hi" | ion --session "$SID" --provider faux --model faux

# 检查 session 的 system prompt 是否含 <dev_servers>
ion rpc --session "$SID" --method get_messages | jq '.data.messages[0].content' | grep -c "dev_servers"
```

**验证点：**
- ✅ system prompt 含 `<dev_servers count="1">`
- ✅ 含 `port="5173"`

#### B2 去重（连续两轮不重复追加）

```bash
# 再发一条消息
echo "hello again" | ion --session "$SID" --provider faux --model faux

# 检查 prompt 里 <dev_servers> 只出现一次
ion rpc --session "$SID" --method get_messages | jq '.data.messages[0].content' | grep -c "dev_servers"
```

**验证点：**
- ✅ `<dev_servers>` 在单次 prompt 里只出现 1 次（signature 去重生效）

### Group C：端口探活 + session 隔离

#### C1 后台探活兜底

```bash
# 真实启动一个 server（后台）
ion rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","input":{"command":"python3 -m http.server 18000 &"}}'

# 等 2 秒让 server listen
sleep 2

# 手动触发探活（把 18000 加进 probe_ports，或用真实端口）
ion rpc --session "$SID" --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"probe","params":{}}'
```

**验证点：**
- ✅ probe 结果含 alive 端口
- ✅ list 含通过 probe 检测到的 server

#### C2 session 隔离

```bash
# 创建第二个 session
SID2=$(ion rpc --method create_session --params '{"agent":"default"}' | jq -r '.data.session_id')

# SID2 的 list 应该是空的（不继承 SID 的检测结果）
ion rpc --session "$SID2" --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"list","params":{}}'
```

**验证点：**
- ✅ SID2 的 `servers` 为空数组（session 隔离）

#### C3 clear 清除

```bash
ion rpc --session "$SID" --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"clear","params":{}}'

ion rpc --session "$SID" --method extension_rpc \
  --params '{"extension":"dev_server_detector","method":"list","params":{}}'
```

**验证点：**
- ✅ clear 后 list 返回空

---

## 6. 注册与集成

### 6.1 在 Agent 初始化时注册

**文件**：`src/agent/mod.rs`（或 worker runtime 初始化处，参照 LSP Extension 注册方式）

```rust
// 参照 lsp_extension 的注册模式
if config.extensions.dev_server_detector.enabled {
    let detector = Arc::new(DevServerDetectorExtension::new(config.extensions.dev_server_detector.clone()));
    manager.register(detector as Arc<dyn Extension>);
}
```

**非 singleton**：`is_singleton()` 返回 `false`（默认值，不用覆写），每个 worker 独立实例，session 状态自动隔离。

### 6.2 Tool Loop Detector 豁免

本扩展不注册任何工具，不涉及循环检测。无需改 `LOOP_EXEMPT_TOOLS`。

### 6.3 config.rs 集成

**文件**：`src/config.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtensionsConfig {
    // ... 现有字段
    pub lsp: LspConfig,
    pub dev_server_detector: DevServerDetectorConfig,  // 新增
}
```

---

## 7. 关键 bug fix 记录

> 本节在实现过程中填充。以下是**预判的坑**（实现时需注意避免）：

### 预判 Bug 1：持锁跨 await 导致死锁

**风险**：`on_tool_execution_end` 里 spawn 后台探活任务时，若持有 `states.lock()` guard 跨 `tokio::spawn`，可能死锁（同 MonitorExtension 的 emit_event 死锁教训）。

**预防**：spawn 前显式 drop guard（已在 §2.2 代码示例中标注 `drop(state_guard)`）。

### 预判 Bug 2：探活任务在 worker 退出后泄漏

**风险**：`tokio::spawn` 的探活任务持有 `Arc<Mutex<HashMap>>`，worker 退出后任务可能还在跑。

**预防**：探活任务用短超时（单端口 2s），总时长有上限（probe_timeout_secs）；session 退出时清理 state（后续可加 `on_session_end` 钩子主动清）。

### 预判 Bug 3：on_system_prompt 拿不到 session_id（✅ 已解决，非问题）

**原担忧**：`on_system_prompt(&self, prompt: &mut String)` 签名里没有 context，无法知道当前是哪个 session。

**确认结论**：**不是问题**。ION 每个 worker 拥有独立的 `ExtensionManager`（`src/agent/agent_loop.rs:507`），worker 级扩展的 `self` 天然绑定当前 session，不需要从参数拿 session_id。参照 Memory（`src/agent/memory.rs:665`）/ Plan / ContextIndex / Bash 扩展的实现，它们的 `on_system_prompt` 都直接用 `self.store`，store 在 worker 初始化时就绑好了。

**设计修正**：原设计的 `HashMap<session_id, SessionState>` 已简化为 `Vec<DetectedServer>` 直接挂在 `self` 上（见 §2.1）。

---

## 8. 后续工作

| # | 待办 | 优先级 |
|---|------|--------|
| 1 | ~~MVP：实现 §1-§3（检测 + 注入 + 去重）~~ ✅ commit `270a392` | ~~P0~~ |
| 2 | ~~后台探活兜底（§2.5）~~ ✅ `probe_and_record` + `tokio::spawn` | ~~P1~~ |
| 3 | ~~framework 推断~~ ✅ `infer_framework()` 从 source_cmd 推断 | ~~P2~~ |
| 4 | **BrowserDevtoolsExtension**（下一步）：本扩展检测到端口后，可选联动 xbrowser 打开浏览器 + 抓 console error 注入。这是本文档的"增强版后续" | P2 |
| 5 | 端口死亡检测优化：定期探活已记录的端口，server 被 kill 后及时从 XML 移除 | P2 |
| 6 | 真实 dev server E2E 测试（起真实 `python -m http.server` 验证探活 + 注入全链路） | P2 |

### 与 BrowserDevtoolsExtension 的关系

本扩展是 **MVP**，只做"感知端口 + 注入提示"。如果验证后发现"LLM 知道有端口但还是不会主动去测"，下一步做 BrowserDevtoolsExtension：

```
DevServerDetector（本扩展）         BrowserDevtoolsExtension（后续）
─────────────────────────          ──────────────────────────────
检测端口 ✅                         检测端口 ✅（复用本扩展）
注入 <dev_servers> XML             注入 <dev_servers> + <browser_errors>
不开浏览器                          开浏览器（xbrowser goto）
不抓 console                        抓 console error（xbrowser console）
event 驱动                          event 驱动 + 定时 poll console
~200 行                             ~400 行
```

**演进路径**：先做 DevServerDetector 验证"端口感知"是否有价值，有价值再叠加浏览器能力。
