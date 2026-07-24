# ION 用户角色体验验证

## 10 个角色 + 业务场景

### 角色 1：新手开发者（Junior Dev）
**场景**：第一次用 ION，想让它帮忙读代码理解项目
**目标**：`ion "读 Cargo.toml 并告诉我项目用了什么依赖"`
**验证点**：agent 能读文件、返回有意义的内容、不超过 5 轮

### 角色 2：资深开发者（Senior Dev）
**场景**：用 ION 做代码审查
**目标**：`ion --agent reviewer "审查 src/agent/agent_loop.rs 的错误处理"`
**验证点**：reviewer agent 正常加载、输出结构化审查报告

### 角色 3：项目经理（PM）
**场景**：了解项目进度和功能
**目标**：`ion "分析这个项目的架构，列出主要模块和它们的功能"`
**验证点**：agent 能理解项目结构、输出清晰的模块列表

### 角色 4：QA 测试工程师
**场景**：跑测试验证功能
**目标**：`ion "运行 cargo test --lib 并报告测试结果"`
**验证点**：bash 工具能跑 cargo test、agent 正确解析结果

### 角色 5：DevOps 工程师
**场景**：用场景 3（serve）做持续服务
**目标**：`ion serve` → `ion rpc --method health` → `ion rpc --method create_session`
**验证点**：serve 正常启动、health 返回 ok、session 创建成功

### 角色 6：安全审计员
**场景**：验证权限系统能拦截危险操作
**目标**：配 deny .env 规则 → `ion rpc --session SID --method call_tool --params '{"tool":"read","args":{"path":".env"}}'`
**验证点**：read .env 被 Permission 拦截

### 角色 7：WASM 扩展开发者
**场景**：安装 WASM 扩展后验证功能
**目标**：创建 `.ion/rules/rust.md` 规则文件 → `ion "审查 src/main.rs"`
**验证点**：rules-engine WASM 注入规则、agent 遵守规则

### 角色 8：多 Agent 编排者
**场景**：用场景 2（host）做多任务编排
**目标**：`ion --host --agent coordinator "读 Cargo.toml 并总结依赖"`
**验证点**：host 模式启动、spawn_worker 工作、事件输出到 stdout

### 角色 9：会话管理用户
**场景**：创建、查看、恢复会话
**目标**：`ion sessions` → `ion history <SID>` → `ion --continue "继续之前的话题"`
**验证点**：sessions 列表正常、history 显示历史、--continue 恢复会话

### 角色 10：自进化维护者
**场景**：跑自测循环验证系统健康
**目标**：`bash scripts/self_test.sh 3`
**验证点**：三场景全绿、无报错、系统健康

## 执行方式

1. **第一轮**：每个角色按顺序跑自己的场景
2. **采集问题**：记录每个角色遇到的错误/异常/体验问题
3. **评估**：ZCode 评估哪些问题值得修
4. **A→B 修复**：派 A→B 修高优先级问题
5. **第二轮**：打乱角色顺序，重新体验
6. **循环**：直到所有角色无问题
