# LSP Extension 设计文档（按 DESIGN_TEMPLATE + CLI_TEST_TEMPLATE）

## 产出文件

1. **`docs/design/LSP_EXTENSION.md`** — 按 DESIGN_TEMPLATE 写完整设计文档
   - 概览 + 能力清单
   - 实现状态核查清单（14 项，像 Bash Extension 的 21 项）
   - §1 配置（config.json 开关 + Diagnostic 结构）
   - §2 主流程（cargo check → 解析 → 注入 context 流程图）
   - §3 RPC 接口规格（lsp_check 工具 + extension_rpc lsp）
   - §4 CLI 测试指南（Group A-E，每个 case 有完整 ion rpc 命令 + 响应 JSON）
   - §5 对标 pi（pi 的 LSP 1657 行 vs ION 精简版 cargo check）
   - §6 后续工作（rust-analyzer / 多语言 / go-to-definition）

2. **`docs/testing/LSP_CLI_TEST.md`** — 按 CLI_TEST_TEMPLATE 写测试用例
   - Group A: 基础功能（cargo check + diagnostics 返回）
   - Group B: 自动注入（write → 检测 → context 注入）
   - Group C: LLM 主动查询（lsp_check 工具）
   - Group D: 边界（无 Cargo.toml / 编译通过 / 超时）
   - Group E: extension_rpc（CLI 直调）

3. **`tests/lsp_ci.sh`** — bash CI 脚本（照搬 BASH_EXTENSION 的 CI 模式）

## 代码产出

4. **`src/lsp_extension.rs`**（~300 行）— LspExtension + LspCheckTool
   - Diagnostic struct（file/line/column/severity/message/code）
   - LspExtension impl Extension（on_tool_execution_end + on_context + on_extension_rpc）
   - LspCheckTool impl Tool（LLM 可主动调用）
   - cargo check JSON 解析（逐行 serde_json，过滤 compiler-message）
   - XML 格式化注入（<diagnostics> block，对齐 Memory 的 <memory_context> 模式）

5. **`src/worker_rpc.rs`** — 注册点（~3 行改动）
6. **`src/bin/ion.rs`** — standalone 注册（~3 行）

## 实现顺序（跟 Bash Extension 一样）

1. 先写设计文档（DESIGN_TEMPLATE）→ 你审
2. 审通过后写代码
3. 写 CLI 测试指南（CLI_TEST_TEMPLATE）
4. 写 CI 脚本
5. 跑 CI 验证

## 与 Bash Extension 对齐的关键点

| 维度 | Bash Extension | LSP Extension |
|------|---------------|---------------|
| 扩展注册 | `ext_reg.register(Box::new(BashExtension::new()))` | `ext_reg.register(Box::new(LspExtension::new()))` |
| LLM 工具 | `bash_run` / `bash_kill` / `bash_send` | `lsp_check` |
| Extension RPC | `list / kill / send / inspect / clean` | `check / clear / status` |
| 事件 | `process_started / completed / killed` | `diagnostics_updated / diagnostics_clean` |
| Context 注入 | `<bash_result>` XML（follow_up） | `<diagnostics>` XML（on_context） |
| 持久化 | processes.json | 不需要（每次实时 cargo check） |
| 配置开关 | config.json extensions.bash.enabled | config.json extensions.lsp.enabled |
| CLI 测试 | Group A-E 18 case | Group A-E ~12 case |

## 对标 pi

| 维度 | pi LSP | ION LSP（本设计） |
|------|--------|-------------------|
| 引擎 | rust-analyzer JSON-RPC（完整 LSP server） | cargo check --message-format=json（精简版） |
| 诊断 | diagnostics + definition + hover + references + rename | 仅 diagnostics（P0） |
| 触发 | 文件保存 + 钩子 | write/edit 工具后 on_tool_execution_end |
| 注入 | agent_end 自动注入 + lsp 工具 | on_context 注入 + lsp_check 工具 |
| 代码量 | 1657 + 402 = ~2060 行 | ~300 行（先做 80% 场景） |
| 多语言 | 通过 LSP server 支持任意语言 | 先只 Rust（后续扩展） |

## 不做的事（明确排除）

- 不做完整 LSP server（rust-analyzer JSON-RPC）——太复杂
- 不做 go-to-definition / hover / rename——先 diagnostics
- 不做其他语言——先 Rust
- 不改 agent_loop.rs 核心——纯扩展 + 工具
