# Message Source Tag — CLI 测试指南

> **用途**：验证 `UserMessage.source` 字段（prompt / steer / followUp / interrupt）在四种消息模式下正确标记，且 list_turns / get_messages 正确返回。

对齐设计文档：[docs/design/MESSAGE_SOURCE_TAG.md](../design/MESSAGE_SOURCE_TAG.md)。

---

## 1. RPC 接口规格

### 1.1 prompt（带 behavior）

**请求：**
```bash
ion rpc --session <sid> --method prompt \
  --params '{"text":"先看 src/","behavior":"steer"}'
```

**请求参数：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `text` | string | 必填 | 消息文本 |
| `behavior` | string | `"interrupt"` | `interrupt` / `steer` / `followUp`（空闲时忽略，当 prompt 处理） |

**响应 JSON（成功）：**
```json
{"type":"response","id":"1","command":"prompt","success":true,"data":null}
```

> prompt 是异步的，立即返回 null。实际 source 标记在后续 list_turns / get_messages 查询时可见。

### 1.2 list_turns（查看 source）

**请求：**
```bash
ion rpc --session <sid> --method list_turns
```

**响应 JSON（含 source）：**
```json
{
  "type": "response",
  "command": "list_turns",
  "success": true,
  "data": {
    "turns": [{
      "turnId": "0",
      "userContent": "帮我分析架构",
      "source": "prompt",
      "assistantContent": "...",
      "durationMs": 3808
    }, {
      "turnId": "1",
      "userContent": "先只看 src/",
      "source": "steer",
      "assistantContent": "...",
      "durationMs": 2100
    }],
    "hasMore": false,
    "totalCount": 2
  }
}
```

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `source` | string? | `prompt` / `steer` / `followUp` / `interrupt`；旧数据可能无此字段 |

### 1.3 get_messages（查看 message 内的 source）

**请求：**
```bash
ion rpc --session <sid> --method get_messages --params '{"view":"full"}'
```

**响应 JSON（含 source）：**
```json
{
  "data": {
    "messages": [{
      "type": "message",
      "message": {
        "User": {
          "role": "user",
          "content": [{"Text": {"text": "先看 src/"}}],
          "source": "steer"
        }
      }
    }]
  }
}
```

---

## 2. 测试前置

```bash
# 编译
cargo build --bin ion --bin ion-worker

# 清理残留 host
pkill -f "target/debug/ion serve" 2>/dev/null || true
rm -f "$HOME/.ion/host.sock"

# 起 host（用 FauxProvider，不走真实 LLM）
ION_FAUX_REPLY="test reply" ./target/debug/ion serve >/tmp/ion_ms_host.log 2>&1 &
HOST_PID=$!
sleep 5
```

---

## 3. Group A：正常 prompt 的 source 标记

> 验证空闲时发 prompt，source = `prompt`。

#### A1 空闲时 prompt 标记为 prompt

```bash
# 1. 创建 session
SID=$(ion rpc --method create_session --params '{"agent":"build"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])")

# 2. 空闲时发 prompt（agent 此时不忙）
ion rpc --session "$SID" --method prompt --params '{"text":"你好"}'
sleep 3

# 3. 查 list_turns
ion rpc --session "$SID" --method list_turns
```

**预期：**
```json
{"turns":[{"turnId":"0","userContent":"你好","source":"prompt",...}]}
```

**验证点：**
- ✅ `source` 字段值为 `"prompt"`
- ✅ userContent = "你好"

#### A2 get_messages 里的 source

```bash
ion rpc --session "$SID" --method get_messages --params '{"view":"full"}'
```

**验证点：**
- ✅ User message 含 `"source":"prompt"`

---

## 4. Group B：steer 插队标记

> 验证 agent 运行中用 behavior=steer，source = `steer`。

#### B1 steer RPC 注入标记

```bash
# 1. 先发一个慢 prompt 让 agent 忙起来（faux 会快速返回，需用 script 模拟多轮）
cat > /tmp/faux_slow.jsonl <<'EOF'
{"text":"正在分析..."}
{"text":"继续"}
EOF

# 重启 host 用 script 模式
kill $HOST_PID; sleep 1; rm -f "$HOME/.ion/host.sock"
ION_FAUX_SCRIPT=/tmp/faux_slow.jsonl ./target/debug/ion serve >/tmp/ion_ms_host.log 2>&1 &
HOST_PID=$!; sleep 5

SID=$(ion rpc --method create_session --params '{"agent":"build"}' \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['data']['session_id'])")

# 2. 发 prompt 触发 agent
ion rpc --session "$SID" --method prompt --params '{"text":"开始分析"}' &
sleep 0.5

# 3. agent 忙时发 steer
ion rpc --session "$SID" --method prompt --params '{"text":"先看 src/","behavior":"steer"}'
sleep 3

# 4. 查 list_turns
ion rpc --session "$SID" --method list_turns
```

**预期：**
```json
{"turns":[
  {"turnId":"0","userContent":"开始分析","source":"prompt",...},
  {"turnId":"1","userContent":"先看 src/","source":"steer",...}
]}
```

**验证点：**
- ✅ 第二条 turn 的 `source` = `"steer"`
- ✅ steer 消息出现在 agent 运行过程中（drain_steering 注入）

#### B2 steer 独立 RPC

```bash
ion rpc --session "$SID" --method steer --params '{"text":"用 grep 搜"}'
sleep 2
ion rpc --session "$SID" --method list_turns
```

**验证点：**
- ✅ 新 turn 的 `source` = `"steer"`

---

## 5. Group C：followUp 追加标记

> 验证 behavior=followUp，source = `followUp`。

#### C1 followUp 在 agent_end 后消费

```bash
# 1. 发 prompt（agent 会跑完后 idle）
ion rpc --session "$SID" --method prompt --params '{"text":"第一轮问题"}'
sleep 0.3

# 2. agent 忙时追加 followUp
ion rpc --session "$SID" --method prompt --params '{"text":"还有个问题","behavior":"followUp"}'
sleep 4

# 3. 查 list_turns
ion rpc --session "$SID" --method list_turns
```

**预期：**
```json
{"turns":[
  {"userContent":"第一轮问题","source":"prompt",...},
  {"userContent":"还有个问题","source":"followUp",...}
]}
```

**验证点：**
- ✅ 第二条 turn 的 `source` = `"followUp"`
- ✅ followUp 消息在第一次 agent_end 后才被消费

#### C2 follow_up 独立 RPC

```bash
ion rpc --session "$SID" --method follow_up --params '{"text":"补充一点"}'
sleep 3
ion rpc --session "$SID" --method list_turns
```

**验证点：**
- ✅ `source` = `"followUp"`

---

## 6. Group D：interrupt 打断标记

> 验证 behavior=interrupt（默认），source = `interrupt`。

#### D1 interrupt 强行打断

```bash
# 1. 发慢 prompt
ion rpc --session "$SID" --method prompt --params '{"text":"长任务"}' &
sleep 0.3

# 2. 强行打断
ion rpc --session "$SID" --method prompt --params '{"text":"别做了","behavior":"interrupt"}'
sleep 3

# 3. 查 list_turns
ion rpc --session "$SID" --method list_turns
```

**预期：**
```json
{"turns":[
  {"userContent":"长任务","source":"prompt","status":"aborted",...},
  {"userContent":"别做了","source":"interrupt",...}
]}
```

**验证点：**
- ✅ 第二条 `source` = `"interrupt"`
- ✅ 第一条 status 可能是 `"aborted"`（被打断了）

#### D2 默认 behavior（无 behavior 参数）= interrupt

```bash
ion rpc --session "$SID" --method prompt --params '{"text":"忙时默认打断"}'  # 无 behavior
sleep 3
ion rpc --session "$SID" --method list_turns
```

**验证点：**
- ✅ agent 忙时无 behavior 参数，`source` = `"interrupt"`

---

## 7. Group E：向后兼容（旧数据）

> 验证旧 session（无 source 字段）不报错。

#### E1 旧 jsonl 无 source 字段

```bash
# 1. 手动构造旧格式 jsonl（无 source）
OLD_DIR=$(mktemp -d)
OLD_SESSION="$OLD_DIR/.ion/sessions/old_sess.jsonl"
mkdir -p "$(dirname "$OLD_SESSION")"
cat > "$OLD_SESSION" <<'JSONL'
{"type":"session","version":3,"id":"old_sess","cwd":"/tmp"}
{"type":"message","id":"m1","message":{"User":{"role":"user","content":[{"Text":{"text":"旧消息"}}]}}}
JSONL

# 2. 用 ion history 读（不应报错）
ion history "$OLD_SESSION"
```

**验证点：**
- ✅ 不崩溃
- ✅ 旧消息正常显示（source 当作 prompt 处理）

#### E2 list_turns 读旧数据 source 为空/默认

**验证点：**
- ✅ `source` 字段不存在或为 `null`（旧数据）
- ✅ UI 兼容处理（当作 prompt）

---

## 8. Group F：jsonl 落盘验证

> 验证 source 字段正确写入磁盘 jsonl。

#### F1 steer 消息落盘含 source

```bash
# 执行 B1 的 steer 操作后，查 jsonl 文件
SESS_FILE=$(find ~/.ion/agent/sessions -name "*.jsonl" -mmin -2 | head -1)
grep '"source":"steer"' "$SESS_FILE"
```

**验证点：**
- ✅ jsonl 里有 `"source":"steer"` 的 message entry

#### F2 prompt 消息落盘含 source

```bash
grep '"source":"prompt"' "$SESS_FILE"
```

**验证点：**
- ✅ 正常 prompt 的 source 也落盘

#### F3 source=None 时不序列化（skip_serializing_if）

```bash
# 旧数据或特殊场景下 source 可能不输出
# 验证：message entry 要么有 "source":"xxx"，要么完全没有 source 字段
# 不应该出现 "source":null
grep '"source":null' "$SESS_FILE"
```

**验证点：**
- ✅ 无 `"source":null`（skip_serializing_if 生效）

---

## 9. 清理

```bash
kill "$HOST_PID" 2>/dev/null || true
wait "$HOST_PID" 2>/dev/null || true
pkill -f "target/debug/ion serve" 2>/dev/null || true
rm -f "$HOME/.ion/host.sock"
rm -rf "$OLD_DIR" /tmp/faux_slow.jsonl
```

---

## 测试统计

| Group | 用例数 | 覆盖 |
|-------|--------|------|
| A 正常 prompt | 2 | source=prompt 标记 + get_messages 返回 |
| B steer 插队 | 2 | behavior=steer + steer RPC |
| C followUp 追加 | 2 | behavior=followUp + follow_up RPC |
| D interrupt 打断 | 2 | behavior=interrupt + 默认 behavior |
| E 向后兼容 | 2 | 旧数据无 source 不报错 |
| F jsonl 落盘 | 3 | source 写入磁盘 + skip_serializing_if |
| **合计** | **13** | 四种 source 全覆盖 |
