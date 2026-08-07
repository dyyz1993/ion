# Task Spec: 删除 ion run_agent CLI 子命令

> **状态：待 B 执行** | 改动范围：`src/bin/ion.rs` | 只删 CLI 入口，不删函数

## 背景

`ion run_agent` 是 8 月 6 日加的 CLI 子命令，用于调试 `run_agent()` 函数。但它不是用户需要的功能——CLI 应该简洁，多一个子命令让用户困惑。

`run_agent()` 函数本身被 hooks 的 agent handler（`src/hooks/handler_runner.rs:374`）使用，**不能删函数**。只删 CLI 子命令。

## 要删的内容

文件 `src/bin/ion.rs`，删以下 3 处：

### 1. 删 Commands enum 里的 RunAgent 变体（约 386 行附近）

找到 `RunAgent {` 开头的 enum 变体，整个删掉。它大概长这样：
```rust
RunAgent {
    #[arg(long)]
    tier: String,
    #[arg(long)]
    system: Option<String>,
    #[arg(long)]
    max_turns: Option<u32>,
    #[arg(long)]
    tools: Option<String>,
    #[arg(long)]
    thinking: Option<String>,
    #[arg(long)]
    schema: Option<String>,
    prompt: String,
},
```

### 2. 删 cmd_run_agent_cli 函数（约 3738-3810 行）

整个 `async fn cmd_run_agent_cli(...)` 函数删掉。

### 3. 删 match 分支（约 4543-4544 行）

```rust
Some(Commands::RunAgent { tier, system, max_turns, tools, thinking, schema, prompt }) => {
    cmd_run_agent_cli(...).await;
}
```

这段删掉。

## 不要删的

- `src/internal_agent.rs` — `run_agent()` 函数定义，hooks 用它
- `src/config.rs` — `query_tier()` 函数，4 个扩展用
- `src/hooks/handler_runner.rs` — hooks 的 agent handler
- CLI 的其他子命令

## 验证

```bash
# 1. 编译通过
cargo check 2>&1 | tail -3

# 2. 确认 run_agent CLI 没了
ion --help 2>&1 | grep run_agent   # 应该无输出

# 3. 确认函数还在（hooks 依赖）
grep -n 'pub async fn run_agent' src/internal_agent.rs   # 应该还在

# 4. 测试通过
cargo test --lib 2>&1 | tail -3
```

## 守门

- ✅ `cargo check` 无错误
- ✅ `ion --help` 不含 run_agent
- ✅ `cargo test --lib` 全过
- ✅ 不删 src/internal_agent.rs 和 src/config.rs
- ✅ 无 U+FFFD
