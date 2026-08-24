# {Extension 名称} 手册

> **状态：开发中** — 一句话说明实现与验证进度。
>
> **类型：** 运行时 WASM Extension
>
> **Extension ID：** `{extension_id}`
>
> **ABI 版本：** `1`
>
> **发行版本：** `{semver}`

## 能力与边界

一句话描述这个 Extension 做什么，以及明确不做什么。

## 构建与安装

```bash
cd {extension_directory}
cargo build --target wasm32-wasip1 --release

# 全局安装
ion extension install ./target/wasm32-wasip1/release/{artifact_name}.wasm

# 或项目级安装
mkdir -p <project>/.ion/extensions
cp ./target/wasm32-wasip1/release/{artifact_name}.wasm \
  <project>/.ion/extensions/
```

## 工具

| 名称 | 参数 JSON Schema | 返回值 | 说明 |
|------|------------------|--------|------|
| `{tool_name}` | `{...}` | `{...}` | 工具作用 |

每个工具补充一条完整的 `call_tool` 命令和成功、失败响应。

## Extension RPC

| method | params | 返回值 | 说明 |
|--------|--------|--------|------|
| `{method}` | `{...}` | `{...}` | 查询或操作说明 |

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"{extension_id}","method":"{method}","params":{}}'
```

## 存储

| 维度 | Key/目录 | 用途 | 保留策略 |
|------|----------|------|----------|
| session / project / project_local / global | `{key}` | ... | ... |

## 事件

| customType | 触发时机 | visibility | data 字段 |
|------------|----------|------------|-----------|
| `{event_type}` | 什么操作后触发 | `llm_and_ui` | `{...}` |

```bash
ion subscribe --session <sid> --extension {extension_id}
```

说明新连接如何 Pull 当前状态，以及一个终端操作后其他终端如何收到关闭或更新事件。

## 配置

列出配置项、默认值、作用域，以及禁用方式。

## 验证

### Harness

```bash
cargo test --test {harness_test}
```

### CLI CI

```bash
bash tests/{extension_id}_ci.sh
```

### 真实 LLM case

```bash
ION_E2E=1 cargo test --test {e2e_test} -- --ignored
```

记录验证日期、模型、预期结果和已知限制。
