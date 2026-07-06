# {扩展名} 扩展手册

> 扩展类型：WASM / Rust Extension
> 构建命令：`cargo build --target wasm32-wasip1 --release`
> 安装路径：`<project>/.ion/extensions/{name}.wasm`

## 能力

一句话描述这个扩展做什么。列表式说明核心能力。

## 工具

| 名称 | 参数 | 说明 |
|------|------|------|
| `{tool_name}` | `{参数 JSON schema}` | 工具作用 |
| ... | ... | ... |

## 存储

| 维度 | 路径 | 用途 |
|------|------|------|
| session | `~/.ion/agent/sessions/{hash}/data/{sid}/{ext_name}/` | ... |
| project | `~/.ion/agent/project-data/{hash}--{name}/{ext_name}/` | ... |
| global | `~/.ion/agent/extensions-data/{ext_name}/` | ... |

## 事件

| customType | 触发时机 | data 字段 |
|-----------|---------|----------|
| `{event_type}` | 什么操作后触发 | `{...}` |

## 测试

```bash
# 1. 启动 Manager
ion manager start

# 2. 创建 session
ion rpc --method create_session --params '{"agent":"developer"}"
# → sess_xxx

# 3. 验证工具
ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"{tool_name}","args":{...}}'

# 4. 验证事件（可选）
ion subscribe --session sess_xxx --extension {plugin_name}
```
