# ION 全功能 E2E 测试规格

> **状态：测试规格定稿** — 100 个 CLI 级测试用例，覆盖全部功能模块
>
> **执行方式**：每个 case 都是完整的 CLI 命令，可独立运行。使用 FauxProvider（确定性，不调真实 LLM）。

## 设计原则

1. **每个 case 给完整 CLI 命令**（照着能跑）
2. **用 FauxProvider 避免真实 LLM**（确定性 + 零 API 成本）
3. **Group 按功能模块分**（一个 Group 覆盖一个子系统）
4. **前置条件写清楚**（需要 mcp-server-everything 的标 📦）
5. **预期输出可验证**（grep 关键字或 JSON 字段）

---

## Group A：基础执行（场景 1 直接执行）— 10 case

> 验证 `ion "xxx"` 直接执行模式的基本能力。

### A1 基本对话
```bash
ION_FAUX_REPLY="hello world" ion "test"
# 预期：输出 "hello world"，进程退出
```
✅ stdout 含 "hello world"

### A2 管道 stdin
```bash
echo "from pipe" | ION_FAUX_REPLY="got pipe" ion
# 预期：自动检测管道，读 stdin 当消息
```
✅ stdout 含 "got pipe"

### A3 --print 非交互模式
```bash
ION_FAUX_REPLY="print mode" ion --print "test"
# 预期：处理完即退（-p 别名）
```
✅ 进程正常退出（exit code 0）

### A4 --json 模式
```bash
ION_FAUX_REPLY='{"result":"ok"}' ion --json "get json"
# 预期：system prompt 注入 JSON 指令
```
✅ stdout 含 JSON 格式输出

### A5 --max-turns 限制
```bash
ION_FAUX_REPLY="limited" ion --max-turns 1 "test"
# 预期：1 轮后停止
```
✅ 只执行 1 轮

### A6 --no-tools 无工具模式
```bash
ION_FAUX_REPLY="no tools" ion --no-tools "hello"
# 预期：无工具调用
```
✅ 正常执行

### A7 --model 三段式语法
```bash
ION_FAUX_REPLY="model test" ion --model faux/test:thinking "test"
# 预期：model 解析正确
```
✅ 不报错

### A8 --model fast 别名
```bash
ION_FAUX_REPLY="fast" ion --model fast "test"
# 预期：tier_models.fast 解析
```
✅ 不报错

### A9 @file 图片引用
```bash
echo "test" > /tmp/test.txt
ION_FAUX_REPLY="file ref" ion "@/tmp/test.txt"
# 预期：读文件内容当消息
```
✅ 不报错

### A10 --verbose 详细日志
```bash
ION_FAUX_REPLY="verbose" ion --verbose "test" 2>&1
# 预期：stderr 含 tracing 日志
```
✅ stderr 含日志

---

## Group B：会话管理 — 12 case

> 验证 session 生命周期。

### B1 创建会话（默认持久化）
```bash
ION_FAUX_REPLY="session test" ion --print "create session"
# 查 ~/.ion/agent/sessions/ 有新文件
```
✅ sessions 目录有新 JSONL

### B2 --no-session 不持久化
```bash
ION_FAUX_REPLY="no persist" ion --no-session --print "test"
```
✅ sessions 目录无新文件

### B3 --continue 继续上次
```bash
# 先创建一个会话
ION_FAUX_REPLY="first" ion --print "hello"
# 再 continue
ION_FAUX_REPLY="continued" ion --continue --print "again"
# 预期：复用上次会话
```
✅ 第二次输出含上次会话上下文

### B4 --resume 恢复指定会话
```bash
SID=$(ion sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
ION_FAUX_REPLY="resumed" ion --resume "$SID" --print "test"
```
✅ 不报错

### B5 --fork 分叉会话
```bash
SID=$(ion sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
ION_FAUX_REPLY="forked" ion --fork "$SID" --print "new branch"
```
✅ 创建新会话（不同 ID）

### B6 --name 会话命名
```bash
ION_FAUX_REPLY="named" ion --name "my-test" --print "test"
# ion sessions 显示名称
```
✅ sessions 列表含 "my-test"

### B7 --export HTML 导出
```bash
SID=$(ion sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
ion --export /tmp/test_export.html --session "$SID"
```
✅ /tmp/test_export.html 存在

### B8 ion sessions 列表
```bash
ion sessions
# 预期：显示会话列表
```
✅ stdout 含 session ID

### B9 ion sessions --json
```bash
ion sessions --json
# 预期：JSON 格式输出
```
✅ 有效 JSON

### B10 ion sessions --all
```bash
ion sessions --all
# 预期：显示所有项目的会话
```
✅ 不报错

### B11 ion history 查看历史
```bash
SID=$(ion sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
ion history "$SID" --limit 5
```
✅ 显示消息历史

### B12 --session-dir 自定义目录
```bash
ION_FAUX_REPLY="custom dir" ion --session-dir /tmp/ion_test_sessions --print "test"
# 预期：会话存到自定义目录
```
✅ /tmp/ion_test_sessions 有文件

---

## Group C：会话树（Session Tree）— 10 case

> 验证 branch/checkout/rollback。

### C1 --branch 从某条消息分叉
```bash
# 先有会话 + entry ID
SID=$(ion sessions --json 2>/dev/null | grep -o 'sess_[a-f0-9]*' | head -1)
ENTRY=$(ion history "$SID" --json 2>/dev/null | grep -o 'ts_[a-f0-9]*' | head -1)
ION_FAUX_REPLY="branched" ion --session "$SID" --branch "$ENTRY" --print "new branch"
```
✅ 创建分支

### C2 --branch-name 命名分支
```bash
ION_FAUX_REPLY="named branch" ion --session "$SID" --branch "$ENTRY" --branch-name "feature-x" --print "test"
```
✅ 分支名 "feature-x" 注册

### C3 --checkout 切换分支
```bash
ION_FAUX_REPLY="checked out" ion --session "$SID" --checkout "feature-x" --print "continue"
```
✅ 切到 feature-x 分支

### C4 --rollback 回滚
```bash
ION_FAUX_REPLY="rolled back" ion --session "$SID" --rollback "$ENTRY" --print "test"
```
✅ 回滚成功

### C5 --rollback-reason 记录原因
```bash
ION_FAUX_REPLY="rollback reason" ion --session "$SID" --rollback "$ENTRY" --rollback-reason "bug fix" --print "test"
```
✅ tombstone 含 reason

### C6 --restore-code 恢复代码
```bash
ION_FAUX_REPLY="restore code" ion --session "$SID" --rollback "$ENTRY" --restore-code --print "test"
```
✅ 文件恢复到回滚点

### C7 ion session tree 展示树
```bash
ion session tree "$SID"
```
✅ 显示树结构

### C8 ion session branches 列出分支
```bash
ion session branches "$SID"
```
✅ 显示分支列表

### C9 --fork-from-leaf 从叶子分叉
```bash
ION_FAUX_REPLY="fork leaf" ion --fork-from-leaf "$SID/$ENTRY" --print "new from leaf"
```
✅ 创建新会话

### C10 navigate_tree RPC
```bash
ion rpc --session "$SID" --method navigate_tree --params '{"direction":"next"}'
```
✅ 返回节点列表

---

## Group D：RPC + Manager 管理 — 15 case

> 验证 ion serve + ion rpc + 事件订阅。

### D1 ion serve 启动
```bash
ion serve &
sleep 3
ion rpc --method list_sessions
# 预期：返回空列表
kill %1
```
✅ host 启动成功

### D2 create_session
```bash
ion serve &; sleep 3
ion rpc --method create_session --params '{"agent":"build"}'
# 预期：返回 session_id
kill %1
```
✅ 返回 sess_ ID

### D3 get_state
```bash
SID=$(...)  # 有 session 后
ion rpc --session "$SID" --method get_state
```
✅ 返回 model/provider/session_id

### D4 prompt 发消息
```bash
ion rpc --session "$SID" --method prompt --params '{"text":"hello"}'
```
✅ 触发 agent 执行

### D5 get_messages
```bash
ion rpc --session "$SID" --method get_messages
```
✅ 返回消息列表

### D6 subscribe 事件流
```bash
# Terminal 1: ion subscribe --session "$SID"
# Terminal 2: ion rpc --session "$SID" --method prompt --params '{"text":"hi"}'
# 预期：Terminal 1 收到 agent_start/text_delta/agent_end 事件
```
✅ 事件流含 agent_start

### D7 channel_send 广播
```bash
ion rpc --method channel_send --params '{"channel":"test","message":"broadcast"}'
```
✅ 不报错

### D8 send_to_worker 点对点
```bash
# 有两个 session 后
ion rpc --method send_to_worker --params '{"target":"<sid2>","text":"direct msg"}'
```
✅ 不报错

### D9 kill 关闭 Worker
```bash
ion rpc --method kill --params '{"worker_id":"<wid>"}'
```
✅ Worker 停止

### D10 list_workers
```bash
ion rpc --method list_workers
```
✅ 返回 Worker 列表

### D11 get_overview
```bash
ion rpc --method get_overview
```
✅ 返回概览 JSON

### D12 serve stop
```bash
ion serve stop
```
✅ host 停止

### D13 serve status
```bash
ion serve status
```
✅ 返回状态

### D14 config show
```bash
ion config show
```
✅ 显示配置

### D15 config set
```bash
ion config set default-model glm-4.7
ion config show  # 确认生效
```
✅ 配置更新

---

## Group E：工具系统 — 12 case

> 验证工具注册/调用/限制。

### E1 call_tool 直接调 read
```bash
echo "test content" > /tmp/e2e_test.txt
ion rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"path":"/tmp/e2e_test.txt"}}'
```
✅ 返回 "test content"

### E2 call_tool 直接调 bash
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo hi"}}'
```
✅ 返回 "hi"

### E3 call_tool 直接调 write
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"write","args":{"path":"/tmp/e2e_write.txt","content":"written"}}'
cat /tmp/e2e_write.txt
```
✅ 文件含 "written"

### E4 call_tool 不存在的工具
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"nonexistent","args":{}}'
```
✅ 返回 tool not found

### E5 get_active_tools
```bash
ion rpc --session "$SID" --method get_active_tools
```
✅ 返回工具列表

### E6 set_active_tools 白名单
```bash
ion rpc --session "$SID" --method set_active_tools --params '{"tools":["read","bash"]}'
ion rpc --session "$SID" --method get_active_tools
```
✅ 只剩 read + bash

### E7 --tools CLI 白名单
```bash
ION_FAUX_REPLY="tools test" ion --tools "read,bash" --print "test"
```
✅ 不报错

### E8 --exclude-tools 黑名单
```bash
ION_FAUX_REPLY="exclude test" ion --exclude-tools "write" --print "test"
```
✅ 不报错

### E9 get_tools 全部工具
```bash
ion rpc --session "$SID" --method get_tools
```
✅ 返回内置工具列表

### E10 extension_rpc 调扩展方法
```bash
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"memory","method":"list"}'
```
✅ 不报错

### E11 register_remote_tool 📦
```bash
ion rpc --session "$SID" --method register_remote_tool --params '{"name":"test_api","url":"http://localhost:19999/test","method":"GET"}'
```
✅ 返回 registered

### E12 unregister_remote_tool 📦
```bash
ion rpc --session "$SID" --method unregister_remote_tool --params '{"name":"test_api"}'
```
✅ 返回 removed

---

## Group F：MCP 系统 — 15 case

> 验证 MCP Phase 1-4 全链路（已有 mcp_ci.sh Group A-J，这里列关键 case）。

### F1 空配置零开销
```bash
echo '{}' > ~/.ion/config.json
ion serve &; sleep 3
SID=$(ion rpc --method create_session --params '{"agent":"build"}' | grep -o 'sess_[a-f0-9]*')
ion rpc --session "$SID" --method get_mcp_servers
# 预期：data: []
```
✅ 空数组

### F2 配置 stdio server
```bash
cat > ~/.ion/config.json << 'EOF'
{"mcp_servers":{"everything":{"command":"mcp-server-everything","disabled":false}}}
EOF
# 重启 serve，get_mcp_servers
```
✅ 含 everything server

### F3 配置 HTTP server
```bash
cat > ~/.ion/config.json << 'EOF'
{"mcp_servers":{"remote":{"type":"streamable-http","url":"http://x/mcp","disabled":true}}}
EOF
```
✅ transport = streamable-http

### F4 toggle 关闭
```bash
ion rpc --session "$SID" --method mcp_toggle_server --params '{"name":"everything","enabled":false}'
```
✅ success: true

### F5 toggle 不存在
```bash
ion rpc --session "$SID" --method mcp_toggle_server --params '{"name":"ghost","enabled":true}'
```
✅ success: false

### F6 restart server
```bash
ion rpc --session "$SID" --method mcp_restart_server --params '{"name":"everything"}'
```
✅ success: true

### F7 真实连接 + 工具发现 📦
```bash
# 等 MCP 连接
sleep 10
ion rpc --session "$SID" --method get_mcp_servers
# 预期：status=connected, tools>0
```
✅ status = connected

### F8 MCP 工具调用 📦
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"mcp__everything__echo","args":{"message":"e2e"}}'
```
✅ 返回含 "e2e"

### F9 权限控制 — 精确 Deny
```bash
cat > ~/.ion/settings.json << 'EOF'
{"permissions":{"rules":[{"id":"p1","provider":"user","subject":"mcp_tool","pattern":"mcp__everything__echo","decision":"Deny","scope":"Project"}]}}
EOF
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"permission","method":"reload"}'
ion rpc --session "$SID" --method call_tool --params '{"tool":"mcp__everything__echo","args":{"message":"blocked"}}'
```
✅ 被拦截

### F10 权限控制 — 通配符 Deny
```bash
cat > ~/.ion/settings.json << 'EOF'
{"permissions":{"rules":[{"id":"p2","provider":"user","subject":"mcp_tool","pattern":"mcp__everything__*","decision":"Deny","scope":"Project"}]}}
EOF
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"permission","method":"reload"}'
```
✅ 所有 mcp__everything__* 被禁

### F11 resources 发现 📦
```bash
ion rpc --session "$SID" --method get_mcp_servers
# 预期：resources 非空
```
✅ resources > 0

### F12 prompts 发现 📦
```bash
ion rpc --session "$SID" --method get_mcp_servers
# 预期：prompts 非空
```
✅ prompts > 0

### F13 read_resource 📦
```bash
ion rpc --session "$SID" --method mcp_read_resource --params '{"server":"everything","uri":"demo://resource/static/document/architecture.md"}'
```
✅ 返回 content

### F14 mcp_reload 热更新
```bash
echo '{}' > ~/.ion/config.json
ion rpc --session "$SID" --method mcp_reload
```
✅ success: true

### F15 方案 C 进程共享 📦
```bash
pgrep -f mcp-server-everything | wc -l
# 预期：1（只 spawn 一份）
```
✅ 进程数 = 1

---

## Group G：Team 编排 — 10 case

> 验证 spawn_worker + channel + worktree。

### G1 spawn_worker 创建子 Worker
```bash
# 用 FauxProvider 模拟 LLM 调 spawn_worker
echo '{"text":"spawning","tool_call":{"name":"spawn_worker","input":{"agent":"developer","task":"test"}}}' > /tmp/faux_spawn.json
export ION_FAUX_SCRIPT=/tmp/faux_spawn.json
ion --host "spawn developer"
unset ION_FAUX_SCRIPT
```
✅ 创建子 Worker

### G2 send_to_worker 跨 Worker 通信
```bash
# 两个 session 后
ion rpc --method send_to_worker --params '{"target":"<sid2>","text":"hello from parent"}'
```
✅ 不报错

### G3 channel_send 广播
```bash
ion rpc --method channel_send --params '{"channel":"team","message":"sync"}'
```
✅ 不报错

### G4 channel_subscribe 订阅
```bash
ion rpc --method channel_subscribe --params '{"channel":"team"}'
```
✅ 不报错

### G5 await_worker 等待子 Worker
```bash
# spawn 后
ion rpc --session "$SID" --method call_tool --params '{"tool":"await_worker","args":{"worker_id":"<wid>"}}'
```
✅ 不报错

### G6 kill_worker 终止
```bash
ion rpc --method kill --params '{"worker_id":"<wid>"}'
```
✅ Worker 停止

### G7 --host 编排 + 自动退出
```bash
ION_FAUX_REPLY="done" ion --host "simple task"
# 预期：跑完即退
```
✅ 进程退出（exit 0）

### G8 worktree 隔离 📦
```bash
echo '{"text":"spawn with worktree","tool_call":{"name":"spawn_worker","input":{"agent":"developer","task":"isolation test","worktree":true}}}' > /tmp/faux_wt.json
export ION_FAUX_SCRIPT=/tmp/faux_wt.json
ion --host "worktree test"
unset ION_FAUX_SCRIPT
```
✅ worktree 创建

### G9 coordinator + developer 编排 📦
```bash
# 真实 LLM + --agent coordinator
ion --host --agent coordinator "让 developer 读 README.md 并总结"
```
✅ coordinator spawn developer

### G10 get_children 查子 Worker
```bash
ion rpc --session "$SID" --method get_children
```
✅ 返回子 Worker 列表

---

## Group H：Memory 系统 — 8 case

> 验证 Memory v0.1 + V0.2 统一存储。

### H1 memory_save 存记忆
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"memory_save","args":{"content":"test memory","description":"e2e","category":"test","tags":["e2e"]}}'
```
✅ 返回 ID

### H2 memory_search 搜记忆
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"memory_search","args":{"query":"test"}}'
```
✅ 返回含 "test memory"

### H3 global_memory_search 跨项目搜
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"global_memory_search","args":{"query":"test"}}'
```
✅ 搜到 v0.1 存的（统一存储）

### H4 global_memory_save 全局存
```bash
ion rpc --session "$SID" --method call_tool --params '{"tool":"global_memory_save","args":{"content":"global mem","project":"ion"}}'
```
✅ 返回 ID

### H5 extension_rpc memory list
```bash
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"memory","method":"list"}'
```
✅ 返回列表

### H6 extension_rpc memory forget
```bash
# 先 save 拿到 ID
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"memory","method":"forget","args":{"id":"<mem_id>"}}'
```
✅ 软删除

### H7 extension_rpc global-memory search
```bash
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"global-memory","method":"search","args":{"query":"test"}}'
```
✅ 返回搜索结果

### H8 extension_rpc global-memory list
```bash
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"global-memory","method":"list"}'
```
✅ 返回列表

---

## Group I：File Snapshot — 8 case

> 验证文件快照 + 差异 + 恢复。

### I1 get_modified_files
```bash
ion rpc --session "$SID" --method get_modified_files
```
✅ 返回修改文件列表

### I2 get_file_diff
```bash
ion rpc --session "$SID" --method get_file_diff --params '{"path":"src/config.rs"}'
```
✅ 返回 diff

### I3 get_batch_diffs
```bash
ion rpc --session "$SID" --method get_batch_diffs --params '{"paths":["src/config.rs","src/mcp/mod.rs"]}'
```
✅ 返回多个 diff

### I4 get_file_history
```bash
ion rpc --session "$SID" --method get_file_history --params '{"path":"src/config.rs"}'
```
✅ 返回历史

### I5 restore_files
```bash
ion rpc --session "$SID" --method restore_files --params '{"turn_id":"<turn_id>"}'
```
✅ 不报错

### I6 review_pending 审批队列
```bash
ion rpc --session "$SID" --method review_pending
```
✅ 返回队列

### I7 review_approve
```bash
ion rpc --session "$SID" --method review_approve --params '{"path":"test.txt"}'
```
✅ 不报错

### I8 review_reject
```bash
ion rpc --session "$SID" --method review_reject --params '{"path":"test.txt"}'
```
✅ 不报错

---

## Group J：权限 + 运行时 — 10 case

> 验证权限引擎 + CommandGuard + 运行时模式。

### J1 set_permission_mode
```bash
ion rpc --session "$SID" --method set_permission_mode --params '{"mode":"whitelist"}'
```
✅ 不报错

### J2 permission Deny 命令
```bash
cat > ~/.ion/settings.json << 'EOF'
{"permissions":{"rules":[{"id":"p1","provider":"user","subject":"command.run","pattern":"rm *","decision":"Deny","scope":"Project"}]}}
EOF
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"permission","method":"reload"}'
ion rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"rm /tmp/test"}}'
```
✅ 被拦截

### J3 permission Allow 文件读
```bash
cat > ~/.ion/settings.json << 'EOF'
{"permissions":{"rules":[{"id":"p2","provider":"user","subject":"file.read","pattern":"/tmp/*","decision":"Allow","scope":"Project"}]}}
EOF
ion rpc --session "$SID" --method extension_rpc --params '{"extension":"permission","method":"reload"}'
```
✅ /tmp/ 下可读

### J4 --local 强制本地
```bash
ION_FAUX_REPLY="local" ion --local --print "test"
```
✅ 不报错

### J5 --remote 强制远程
```bash
ION_FAUX_REPLY="remote" ion --remote --print "test"
# 预期：尝试连远程（可能失败但不应 panic）
```
✅ 不 panic

### J6 --local + --remote 冲突
```bash
ion --local --remote "test" 2>&1
# 预期：报冲突错误
```
✅ 报错

### J7 get_commands 命令列表
```bash
ion rpc --session "$SID" --method get_commands
```
✅ 返回命令列表

### J8 get_skills 技能列表
```bash
ion rpc --session "$SID" --method get_skills
```
✅ 返回技能列表

### J9 get_extensions 扩展列表
```bash
ion rpc --session "$SID" --method get_extensions
```
✅ 返回扩展列表

### J10 get_settings 配置读取
```bash
ion rpc --session "$SID" --method get_settings
```
✅ 返回配置 JSON

---

## Group K：Compaction + 消息拉取 — 10 case

> 验证会话压缩 + 分页查询。

### K1 compact 手动压缩
```bash
ion rpc --session "$SID" --method compact
```
✅ 不报错

### K2 set_auto_compaction
```bash
ion rpc --session "$SID" --method set_auto_compaction --params '{"enabled":true,"threshold":50000}'
```
✅ 不报错

### K3 get_context_usage
```bash
ion rpc --session "$SID" --method get_context_usage
```
✅ 返回 token 估算

### K4 get_messages 分页
```bash
ion rpc --session "$SID" --method get_messages --params '{"limit":5}'
```
✅ 最多返回 5 条

### K5 get_messages since_compaction 视图
```bash
ion rpc --session "$SID" --method get_messages --params '{"view":"since_compaction"}'
```
✅ 只返回压缩后的

### K6 list_turns 逐轮概览
```bash
ion rpc --session "$SID" --method list_turns
```
✅ 返回轮次列表

### K7 get_turn_detail 单轮明细
```bash
ion rpc --session "$SID" --method get_turn_detail --params '{"turn_id":"<turn_id>"}'
```
✅ 返回详情

### K8 list_inputs 输入列表
```bash
ion rpc --session "$SID" --method list_inputs
```
✅ 返回用户输入列表

### K9 get_session_stats 统计
```bash
ion rpc --session "$SID" --method get_session_stats
```
✅ 返回统计 JSON

### K10 get_system_prompt
```bash
ion rpc --session "$SID" --method get_system_prompt
```
✅ 返回 system prompt

---

## Group L：Workflow + 扩展系统 — 10 case

> 验证 Workflow Engine + WASM 扩展 + Flags。

### L1 workflow validate
```bash
ion workflow validate examples/workflows/delivery.wf.yaml
```
✅ 校验通过

### L2 workflow status
```bash
ion workflow status examples/workflows/delivery.wf.yaml
```
✅ 返回状态

### L3 get_tier_models
```bash
ion rpc --session "$SID" --method get_tier_models
```
✅ 返回 fast/pro/max

### L4 set_tier_models
```bash
ion rpc --session "$SID" --method set_tier_models --params '{"models":{"custom":"zai/glm-4.7"}}'
```
✅ 不报错

### L5 get_flags
```bash
ion rpc --session "$SID" --method get_flags
```
✅ 返回 flag 列表

### L6 set_flag
```bash
ion rpc --session "$SID" --method set_flag --params '{"extension":"memory","flag":"debug","value":true}'
```
✅ 不报错

### L7 extension_add 热加载 WASM 📦
```bash
# 需要 .wasm 文件
ion rpc --session "$SID" --method extension_add --params '{"path":"/path/to/ext.wasm"}'
```
✅ 不报错

### L8 extension_list
```bash
ion rpc --session "$SID" --method extension_list
```
✅ 返回扩展列表

### L9 get_agents
```bash
ion rpc --session "$SID" --method get_agents
```
✅ 返回 agent 列表

### L10 list-agents CLI
```bash
ion list-agents
```
✅ 显示 agent 列表

---

## 用例覆盖矩阵

| Group | 功能模块 | case 数 | 已有 CI 覆盖 |
|-------|---------|--------|------------|
| **A** | 基础执行（场景 1） | 10 | 部分（cli_alignment） |
| **B** | 会话管理 | 12 | 部分（sessions_ci） |
| **C** | 会话树 | 10 | 有（session_tree_verify） |
| **D** | RPC + Manager | 15 | 部分（scenario2） |
| **E** | 工具系统 | 12 | 散落各 CI |
| **F** | MCP 系统 | 15 | 有（mcp_ci A-J） |
| **G** | Team 编排 | 10 | 部分（p4_extension） |
| **H** | Memory 系统 | 8 | 有（global_memory_ci） |
| **I** | File Snapshot | 8 | 有（file_snapshot_ci） |
| **J** | 权限 + 运行时 | 10 | 有（permission_ci） |
| **K** | Compaction + 消息 | 10 | 有（compaction_ci + message_retrieval） |
| **L** | Workflow + 扩展 | 10 | 有（workflow_ci + extension_flags） |
| **合计** | | **130** | |

> 注：原定 100 个，实际细化到 130 个覆盖更完整。标 📦 的需要额外依赖（mcp-server-everything / .wasm 文件）。

---

## 执行计划

### Phase 1：写自动化脚本（每个 Group 一个 .sh）

```
tests/e2e/
├── group_a_basic.sh         ← 基础执行
├── group_b_session.sh       ← 会话管理
├── group_c_tree.sh          ← 会话树
├── group_d_rpc.sh           ← RPC + Manager
├── group_e_tools.sh         ← 工具系统
├── group_f_mcp.sh           ← MCP（复用 mcp_ci.sh）
├── group_g_team.sh          ← Team 编排
├── group_h_memory.sh        ← Memory
├── group_i_snapshot.sh      ← File Snapshot
├── group_j_permission.sh    ← 权限
├── group_k_compaction.sh    ← Compaction + 消息
├── group_l_workflow.sh      ← Workflow + 扩展
└── run_all.sh               ← 一键跑全部
```

### Phase 2：逐个 Group 实现脚本

每个脚本：
1. 用 `ION_FAUX_REPLY` / `ION_FAUX_SCRIPT` 避免真实 LLM
2. 用临时 `TEST_HOME` 隔离测试环境
3. 每个 case 独立（不依赖前一个）
4. 输出 PASS/FAIL/SKIP 统计

### Phase 3：一键回归

```bash
bash tests/e2e/run_all.sh
# 输出：Group A: 10/10 ✅, Group B: 12/12 ✅, ... 总计 130/130 ✅
```
