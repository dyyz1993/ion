# todo-plugin 插件手册

> 插件类型：WASM（`todo-plugin/src/lib.rs`）
> 构建：`cargo build --target wasm32-wasip1 --release -p todo-plugin`
> 安装：`cp target/wasm32-wasip1/release/todo_plugin.wasm <project>/.ion/extensions/`

## 能力

- 管理待办任务（增删改查）
- 数据按 session 维度持久化

## 工具

| 名称 | 参数 | 说明 |
|------|------|------|
| `todo_add` | `{text}` | 创建任务 |
| `todo_list` | `{status?}` | 列表（all/active/done） |
| `todo_done` | `{id}` | 完成 |
| `todo_remove` | `{id}` | 删除 |
| `todo_clean` | `{}` | 清理已完成 |

## 存储

- 维度：session
- 路径：`~/.ion/agent/sessions/{hash}/data/{sid}/todo-plugin/tasks`

## 事件

| customType | 说明 |
|-----------|------|
| `todo_added` | 任务创建 |
| `todo_done` | 任务完成 |
| `todo_removed` | 任务删除 |

## 测试

```bash
ion rpc --method create_session --params '{"agent":"developer"}'
# → sess_xxx

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_add","args":{"text":"测试任务"}}'

ion rpc --session sess_xxx --method call_tool \
  --params '{"tool":"todo_list","args":{"status":"all"}}'
```
