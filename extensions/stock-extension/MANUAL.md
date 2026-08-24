# stock-extension 手册

> **状态：开发中** — 最小工具与 channel host function 示例已实现，行情数据仍为固定演示值。
>
> **类型：** 运行时 WASM Extension
> **Extension ID：** `stock_extension`
> **ABI 版本：** `1`
> **发行版本：** `0.1.0`

## 能力与边界

提供 `get_stock_price` 教学工具并演示 `host_channel_send`。当前不访问真实行情服务，不能用于交易决策。

## 构建与安装

```bash
cd extensions/stock-extension
cargo build --target wasm32-wasip1 --release
ion extension install ./target/wasm32-wasip1/release/stock_extension.wasm
```

## 工具

| 名称 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `get_stock_price` | `{"ticker":"AAPL"}` | `{symbol,price,source}` | 返回固定演示行情 |

## 事件与存储

调用工具时向 `stock-updates` channel 发送演示消息；不使用持久化存储。

## 验证

在接入真实数据源前，需要补 FauxProvider Harness、CLI CI 和 ignored 真实 case。
