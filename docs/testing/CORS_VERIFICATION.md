# CORS 修复验证报告

> **状态：已验证** — 用 curl 断言实测（非仅代码审阅），修复目标面全部通过。

## 验证方法

每个网关起真实进程，发带 `Origin` 头的 OPTIONS 预检 + POST/GET 请求，断言：
1. 预检返回 `204`
2. 响应含 `Access-Control-Allow-Origin`

## 结果

| 网关 | 预检 | ACAO 头 | 判定 |
|---|---|---|---|
| ion-orbit-ui/workspace_gateway.py | 204 ✅ | ✅（预检+POST 均带 `*`） | **PASS** |
| dashboard/web/gateway.cjs | 不适用（简单 GET 无预检） | ✅ | **PASS** |
| ion serve 本体 | 不适用（仅 Unix socket，无 HTTP 面） | — | 不适用 |
| ion-webui/gateway.py (8790) | 404 | 无 | 遗留（同源部署，无跨源前端，暂不需要） |
| approval_push_bridge.py 审批页 (8793) | 501 | 无 | 遗留（`<a href>` 同源导航，无 XHR，不需要） |

## 断言记录（实测输出）

```
PASS 预检204
PASS ACAO: Access-Control-Allow-Origin: *
PASS POST-ACAO: Access-Control-Allow-Origin: *
PASS dashboard-gateway ACAO: Access-Control-Allow-Origin: *
```

## 备注

- Cargo.toml 的 `tower-http cors` feature 当前无消费方（ion serve 无 HTTP 面），属冗余依赖，可择机移除。
- ion-webui 如未来拆分独立前端 origin，需在 FastAPI 挂 `CORSMiddleware`。
