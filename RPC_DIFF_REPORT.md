# ion-worker vs pi RPC 差异报告

## 测试环境
- **pi**: `node dist/cli.js --mode rpc --no-session --no-extensions`
- **ion**: `./target/debug/ion-worker --mode rpc --session rpc-compare`

---

## 一、响应格式差异

### pi 的响应格式

```json
{
  "id": "1",
  "type": "response",        ← 固定 "response"
  "command": "get_state",     ← 回显命令名
  "success": true,            ← 布尔成功标志
  "data": { ... }             ← 结果在 data 字段里
}
```

### ion 的响应格式

```json
{
  "id": "1",
  "type": "success",          ← "success" 或 "error"
  "result": { ... }           ← 结果在 result 字段里
}
```

### 差异清单

| 字段 | pi | ion | 需改 |
|------|-----|-----|------|
| type | `"response"` | `"success"` | ✅ 改为 `"response"` |
| command | `"get_state"` 回显 | ❌ 缺失 | ✅ 加 command 字段 |
| success | `true/false` | ❌ 用 type 区分 | ✅ 加 success 字段 |
| data | 结果数据 | ❌ 用 result | ✅ 改 result → data |
| 错误格式 | `"success":false,"error":"..."` | `"type":"error","error":{...}` | ✅ 对齐 |

---

## 二、Ready 信号差异

### pi
```json
{"type":"ready"}
```

### ion
```json
{"type":"ready","session":"rpc-compare","model":"deepseek-v4-flash","provider":"opencode","channels":[],"version":"0.1.0"}
```

**评估**: ion 的 ready 更丰富。但应该保持兼容，pi 的 RpcClient 只检查 `type === "ready"`，多余字段会被忽略。

**结论**: ✅ 兼容，不需改。

---

## 三、get_state 差异

### pi 返回字段
```json
{
  "model": { "id": "glm-5.2", "name": "...", "api": "...", "provider": "...", ... },
  "thinkingLevel": "high",
  "isStreaming": false,
  "isCompacting": false,
  "steeringMode": "all",
  "followUpMode": "one-at-a-time",
  "permissionMode": "normal",
  "sessionId": "uuid",
  "autoCompactionEnabled": true,
  "messageCount": 0,
  "pendingMessageCount": 0,
  "pendingUIRequests": []
}
```

### ion 返回字段
```json
{
  "is_running": false,
  "message_count": 1,
  "model": "deepseek-v4-flash",      ← 只是字符串，不是对象
  "provider": "opencode",
  "session_id": "rpc-compare"
}
```

### 差异

| 字段 | pi | ion | 需改 |
|------|-----|-----|------|
| model | 完整对象 `{id,name,api,...}` | 字符串 | ✅ 改为对象 |
| thinkingLevel | `"high"` | ❌ 缺失 | ✅ 加 |
| isStreaming | `false` | `is_running` | ✅ 改名 |
| isCompacting | `false` | ❌ 缺失 | ✅ 加 |
| steeringMode | `"all"` | ❌ 缺失 | ✅ 加 |
| followUpMode | `"one-at-a-time"` | ❌ 缺失 | ✅ 加 |
| permissionMode | `"normal"` | ❌ 缺失 | ✅ 加 |
| sessionId | `"uuid"` | `session_id` | ✅ 改 camelCase |
| autoCompactionEnabled | `true` | ❌ 缺失 | ✅ 加 |
| messageCount | `0` | `message_count` | ✅ 改 camelCase |
| pendingMessageCount | `0` | ❌ 缺失 | ✅ 加 |
| pendingUIRequests | `[]` | ❌ 缺失 | ✅ 加 |
| provider | ❌ 缺失 | 有 | 保留 |

---

## 四、get_session_stats 差异

### pi
```json
{
  "sessionId": "...",
  "userMessages": 0,
  "assistantMessages": 0,
  "toolCalls": 0,
  "toolResults": 0,
  "totalMessages": 0,
  "tokens": { "input":0, "output":0, "cacheRead":0, "cacheWrite":0, "total":0 },
  "cost": 0,
  "contextUsage": {
    "tokens": 21040,
    "contextWindow": 1000000,
    "percent": 2.104,
    "breakdown": [ ... 16 个分类 ... ]
  }
}
```

### ion
```json
{
  "message_count": 1,
  "session_id": "rpc-compare",
  "tokens_input": 0,
  "tokens_output": 0
}
```

### 差异

| 字段 | pi | ion | 需改 |
|------|-----|-----|------|
| sessionId | camelCase | snake_case | ✅ |
| userMessages | 分类统计 | ❌ 缺失 | ✅ |
| assistantMessages | 分类统计 | ❌ 缺失 | ✅ |
| toolCalls | 统计 | ❌ 缺失 | ✅ |
| toolResults | 统计 | ❌ 缺失 | ✅ |
| totalMessages | 总数 | ❌ 缺失 | ✅ |
| tokens | 对象 `{input,output,...}` | 扁平字段 | ✅ 改为对象 |
| cost | 0 | ❌ 缺失 | ✅ |
| contextUsage | 16 类分类统计 | ❌ 缺失 | ✅ |

---

## 五、get_last_assistant_text 差异

### pi
```json
{ "data": {} }
```

### ion
```json
{ "result": "" }
```

**结论**: 格式基本一致，只是 data vs result 的问题（全局问题）。

---

## 六、get_tools 差异

### pi
```json
{
  "data": { "tools": [] }
}
```

### ion
```json
{
  "result": [{ "name": "read" }, { "name": "write" }, ...]
}
```

### 差异

| 字段 | pi | ion | 需改 |
|------|-----|-----|------|
| 结构 | `{ "tools": [...] }` | 直接数组 | ✅ 包一层 tools |
| 工具定义 | 完整 ToolDefinition | 只有 name | ✅ 补全 |

---

## 七、dispose 差异

### pi
```json
{ "type":"response","command":"dispose","success":false,"error":"Unknown command: dispose" }
```
pi 的关闭命令实际上是 `stop` 或 stdin 关闭。

### ion
```json
{ "type":"success","result":null }
```

**结论**: pi 没有 `dispose`，用 `stop`。ion 的 `shutdown`/`dispose`/`kill` 都能关。

---

## 八、全局命名规范差异

| pi 风格 | ion 风格 | 说明 |
|---------|---------|------|
| camelCase | snake_case | pi 全用 camelCase |
| `"type":"response"` | `"type":"success"` | 响应类型 |
| `"data":{...}` | `"result":{...}` | 数据字段名 |
| `"command":"get_state"` | ❌ 缺失 | 回显命令名 |
| `"success":true` | ❌ 缺失 | 成功标志 |

---

## 九、行动清单

### 必须改的（影响兼容性）

1. **响应格式统一为 pi 的格式**:
   ```json
   {"id":"1","type":"response","command":"get_state","success":true,"data":{...}}
   ```

2. **所有字段名改 camelCase**

3. **get_state 补全缺失字段**

4. **get_session_stats 补全完整统计**

5. **get_tools 返回完整工具定义**

### 可以不改的

- ready 信号（pi 忽略多余字段）
- dispose/shutdown（pi 也不标准）

---

## 十、结论

ion 的 RPC 基本框架是对的，但**响应格式需要完全对齐 pi**。核心差异在三个地方：

1. `type: "success"` → `type: "response"`
2. `result: {...}` → `data: {...}`
3. 缺 `command` 和 `success` 字段

改完这三点 + camelCase + 补全字段，就能和 pi 的 RpcClient 完全互通。
