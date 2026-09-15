#!/usr/bin/env python3
"""Generate JSON Schema (draft 2020-12) contracts for ION RPC config domain.

Output: schemas/rpc/config/<command>.json + schemas/rpc/config/_index.json
Run: python3 scripts/gen_rpc_config_schemas.py
"""
import json
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "schemas", "rpc", "config")

ENVELOPE_NOTE = (
    "ION worker RPC 响应信封：{id, type:'response', command, success, data|error}。"
    "success=true → data；success=false → error（output_error_response）。"
)

SECRET_KEY_PATTERN = "^(api_key|api-key|apikey|x-api-key|token|access_token|refresh_token|auth_token|session_token|secret|client_secret|password|passwd|authorization)$"
MASKED = {"oneOf": [{"const": "***"}, {"type": "null"}]}
MASKED_STR = {"const": "***"}


def err_obj():
    return {"type": "object", "required": ["error"],
            "properties": {"error": {"type": "string"}},
            "additionalProperties": True}


def envelope(cmd, data_schema, desc, command_const=None):
    cc = command_const or cmd
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": f"schemas/rpc/config/{cmd}.json",
        "title": f"RPC {cmd} response",
        "description": desc + " " + ENVELOPE_NOTE,
        "type": "object",
        "required": ["id", "type", "command", "success"],
        "properties": {
            "id": {"type": "string"},
            "type": {"const": "response"},
            "command": {"const": cc},
            "success": {"type": "boolean"},
        },
        "oneOf": [
            {"properties": {"success": {"const": True}, "data": data_schema},
             "required": ["success", "data"]},
            {"properties": {"success": {"const": False},
                            "error": {"type": "string"}},
             "required": ["success", "error"]},
        ],
        "additionalProperties": True,
    }


def one_of(*variants):
    return {"oneOf": list(variants)}


def obj(props=None, req=None, additional=True):
    o = {"type": "object", "additionalProperties": additional}
    if props:
        o["properties"] = props
    if req:
        o["required"] = req
    return o


def arr(items):
    return {"type": "array", "items": items}


S = "string"
RULE = obj({
    "id": {"type": S}, "subject": {"type": S}, "pattern": {"type": S},
    "decision": {"type": S}, "scope": {"type": S},
    "provider": {"type": S}, "source": {"type": S, "enum": ["config", "stored"]},
    "createdAt": {"type": [S, "integer", "null"]},
}, req=["id", "subject", "pattern", "decision", "scope", "provider", "source"])

SERVER = obj({
    "name": {"type": S},
    "transport": {"type": S},
    "status": {"type": S, "enum": ["disconnected", "connecting", "connected", "error"]},
    "disabled": {"type": "boolean"},
    "tools": arr(obj({"full_name": {"type": S}, "original_name": {"type": S},
                      "description": {"type": S}}, req=["full_name", "original_name"])),
    "resources": {},
    "prompts": {},
    "error": {"type": [S, "null"]},
}, req=["name", "transport", "status", "disabled", "tools", "resources", "prompts", "error"])

# get_settings 全量：脱敏后的 IonConfig。密钥位必须反映脱敏形态。
GET_SETTINGS_FULL = obj({
    "default_provider": {"type": [S, "null"]},
    "default_model": {"type": [S, "null"]},
    "api_key": dict(MASKED),
    "base_url": {"type": [S, "null"]},
    "provider_api_keys": {"type": "object",
                          "additionalProperties": {"oneOf": [{"const": "***"}, {"type": "null"}]}},
    "providers": {"type": "object", "additionalProperties": obj({
        "name": {"type": S}, "api": {"type": S}, "base_url": {"type": S},
        "api_key": dict(MASKED),
        "headers": {"type": "object", "additionalProperties": dict(MASKED)},
        "models": {"type": "array"},
        "model_overrides": {"type": ["object", "null"]},
    })},
    "extensions": {"type": "object"},
    "tier_models": {"type": "object", "additionalProperties": {"type": S}},
    "security_mode": {"type": [S, "null"]},
    "mcp_servers": {"type": "object", "additionalProperties": obj({
        "command": {"type": S}, "args": {"type": "array"}, "url": {"type": S},
        "type": {"type": S},
        "env": {"type": "object", "additionalProperties": dict(MASKED)},
        "headers": {"type": "object", "additionalProperties": dict(MASKED)},
    })},
    "runtime": {"type": "object"},
    "session": {"type": "object"},
    "skills": {"type": "object"},
    "fetch": {"type": "object"},
    "remote_workers": {"type": ["object", "null"]},
}, req=[])

SPECS = {}

# ── group: config/model ─────────────────────────────────────────────
# 键值查询分支：key=api_key 时 value 必须是脱敏形态（'***' 或 ''），其他 key 不限
GET_SETTINGS_KEYED = {
    "type": "object",
    "required": ["key", "value"],
    "properties": {"key": {"type": S}, "value": {}},
    "if": {"properties": {"key": {"const": "api_key"}}, "required": ["key"]},
    "then": {"properties": {"value": {"enum": ["***", ""]}}},
}
# 用 anyOf 而非 oneOf：FULL 无 required 且 additionalProperties 放行，keyed 实例天然也落进
# FULL——oneOf 的"恰好一个"语义会对合法响应误判；anyOf（至少一个）才是两种形态并集的忠实编码。
SPECS["get_settings"] = ("config", {"anyOf": [GET_SETTINGS_FULL, GET_SETTINGS_KEYED]},
    "读取配置。无 key 参数 → 全量 redacted_value()（脱敏形态：api_key/provider_api_keys/env/headers 全部 '***' 或 null，schema 强制密钥位不得出现明文）；带 key → {key, value}（key=api_key 时 value 强制 '***' 或 ''，if/then 编码）。")

SPECS["set_settings"] = ("config", one_of(
    obj({"key": {"type": S}, "old_value": {}, "new_value": {},
         "saved": {"const": True}}, req=["key", "old_value", "new_value", "saved"]),
    err_obj(),
), "写入单键配置（default_provider/default_model/api_key/base_url）。api_key 的 new_value 脱敏为 '***'。未知 key → data.error。")

SPECS["set_model"] = ("config",
                      obj({"model": {"type": S}, "provider": {"type": S}}, req=["model", "provider"]),
                      "切换模型。⚠️ 响应信封 command 字段是 'get_state'（历史怪癖，不是 'set_model'）。同时落 session JSONL model_change + SessionIndex + SettingsChanged 事件。",
                      {"command_const": "get_state"})

SPECS["cycle_model"] = ("config", obj({
    "modelId": {"type": S}, "provider": {"type": S},
    "previousModel": {"type": S},
    "note": {"type": S},
}, req=["modelId", "provider"]), "循环切换当前 provider 的模型。单模型时带 note 不带 previousModel；正常时带 previousModel 不带 note。")

SPECS["get_available_models"] = ("config", arr(obj({
    "id": {"type": S}, "name": {"type": S}, "provider": {"type": S},
    "reasoning": {"type": "boolean"}, "contextWindow": {"type": "integer"},
}, req=["id", "name", "provider", "reasoning", "contextWindow"])),
    "列出可用模型。⚠️ data 是裸数组（不包对象壳）。host 级直答（不需要 worker）。")

SPECS["get_tier_models"] = ("config",
                            {"type": "object", "additionalProperties": {"type": S}},
                            "读 tier 别名映射（fast/pro/max → 'provider/model'）。⚠️ data 是裸 map。")

SPECS["set_tier_models"] = ("config", one_of(
    obj({"tier": {"type": S}, "oldModel": {"type": [S, "null"]},
         "newModel": {"type": S}, "saved": {"const": True}},
        req=["tier", "oldModel", "newModel", "saved"]),
    err_obj(),
), "写 tier 别名。缺 tier/model → data.error；save 失败 → data.error。")

SPECS["set_thinking_level"] = ("config",
                               obj({"thinkingLevel": {"type": S}}, req=["thinkingLevel"]),
                               "设置思考级别（off/minimal/low/medium/high/xhigh）+ SettingsChanged 事件。")

SPECS["cycle_thinking_level"] = ("config",
                                 obj({"thinkingLevel": {"type": S}, "previousLevel": {"type": S}},
                                     req=["thinkingLevel", "previousLevel"]),
                                 "循环切换思考级别，写 SessionIndex。")

SPECS["set_cwd"] = ("config", one_of(
    obj({"cwd": {"type": S}, "success": {"const": True}}, req=["cwd", "success"]),
    err_obj(),
), "切换工作目录。路径不存在或缺参 → data.error。同步 SessionIndex.last_cwd。")

SPECS["add_dir"] = ("config", one_of(
    obj({"added": {"type": "boolean"}, "dir": {"type": S},
         "extra_cwds": arr({"type": S})}, req=["added", "dir", "extra_cwds"]),
    err_obj(),
), "添加额外工作目录（参数 dir|cwd|path 均可）。缺参/路径不存在 → data.error。")

SPECS["remove_dir"] = ("config", one_of(
    obj({"removed": {"type": "boolean"}, "extra_cwds": arr({"type": S})},
        req=["removed", "extra_cwds"]),
    err_obj(),
), "移除额外工作目录。")

SPECS["list_dirs"] = ("config",
                      obj({"cwd": {"type": [S, "null"]}, "extra_cwds": arr({"type": S})},
                          req=["cwd", "extra_cwds"]),
                      "列出工作目录（cwd + extra_cwds）。")

SPECS["set_permission_mode"] = ("config", one_of(
    obj({"mode": {"type": S}, "success": {"const": True}}, req=["mode", "success"]),
    err_obj(),
), "切命令守卫模式（open/blacklist/whitelist）。缺参或非法 mode → data.error。+ SettingsChanged 事件。")

SPECS["set_auto_compaction"] = ("config",
                                obj({"autoCompaction": {"type": "boolean"}}, req=["autoCompaction"]),
                                "开关自动压缩。")

SPECS["set_auto_retry"] = ("config",
                           obj({"enabled": {"type": "boolean"}, "max_retries": {"type": "integer"}},
                               req=["enabled", "max_retries"]),
                           "开关 LLM 自动重试。enabled=false 时 max_retries 恒 0。")

# ── group: approval (file-approval) ─────────────────────────────────
# ⚠️ 双路径形状分裂（同 review_pending 两个实现点）：
#   空闲路径（worker_rpc.rs 主 match）summary={total,added,modified,deleted}；
#   agent.run 期间的只读 bg 路径（select! 分支）summary={total}，且 status 经
#   format!("{:?}") 序列化 String → 带字面引号（"\"added\""）。
QSTAT = {"type": S,
         "enum": ["added", "modified", "deleted",
                  '"added"', '"modified"', '"deleted"']}
SPECS["review_pending"] = ("approval", one_of(
    obj({"pending": arr(obj({"path": {"type": S}, "status": QSTAT,
                             "diffStat": {"type": S}}, req=["path", "status", "diffStat"])),
         "summary": obj({"total": {"type": "integer"}, "added": {"type": "integer"},
                         "modified": {"type": "integer"}, "deleted": {"type": "integer"}},
                        req=["total"])},
        req=["pending", "summary"]),
    err_obj(),
), "列出待审文件（只回摘要：path/status/diffStat，不含内容，大 diff 走 review_file_diff）。⚠️ 双路径形状分裂：空闲路径 summary 含 total/added/modified/deleted；agent.run 期间经只读 bg 通道 summary 只有 total（status 带字面引号）。file-snapshot 未启用 → data.error='approval not enabled'。")

SPECS["review_file_diff"] = ("approval", one_of(
    obj({"path": {"type": S}, "status": {"type": S}, "diff": {"type": S},
         "diffAvailable": {"type": "boolean"}, "added": {"type": "integer"},
         "removed": {"type": "integer"}},
        req=["path", "status", "diff", "diffAvailable", "added", "removed"]),
    err_obj(),
), "单文件完整 diff（tree 快照 + baseline 语义）。缺 path / 不在 pending → data.error。")

SPECS["review_approve"] = ("approval", one_of(
    obj({"path": {"type": S}, "status": {"const": "approved"},
         "approvedTreeHash": {"type": [S, "null"]}}, req=["path", "status", "approvedTreeHash"]),
    err_obj(),
), "批准单文件。")

SPECS["review_reject"] = ("approval", one_of(
    obj({"path": {"type": S}, "status": {"const": "rejected"}, "action": {"type": S},
         "rolledBack": {"const": True}, "denyMessageInjected": {"type": "boolean"}},
        req=["path", "status", "action", "rolledBack", "denyMessageInjected"]),
    err_obj(),
), "拒绝单文件并回滚到 baseline，注入 <approval_feedback> XML deny 消息进 session JSONL。")

SPECS["review_approve_all"] = ("approval", one_of(
    obj({"approved": {"type": "integer"}, "errors": {"type": "integer"},
         "total": {"type": "integer"},
         "failures": arr(obj({"index": {"type": "integer"}, "error": {"type": S}},
                             req=["index", "error"]))},
        req=["approved", "errors", "total", "failures"]),
    err_obj(),
), "批准全部 pending（failures 含部分失败明细）。")

SPECS["review_reject_all"] = ("approval", one_of(
    obj({"rejected": {"type": "integer"}, "errors": {"type": "integer"},
         "total": {"type": "integer"}, "denyMessageInjected": {"type": "boolean"}},
        req=["rejected", "errors", "total", "denyMessageInjected"]),
    err_obj(),
), "拒绝全部并批量回滚 + 单条批量 deny 消息注入。")

SPECS["review_approvals"] = ("approval", one_of(
    obj({"approvals": arr(obj({"path": {"type": S},
                               "status": {"type": S, "enum": ["pending", "approved", "rejected"]},
                               "timestamp": {"type": [S, "integer"]},
                               "approvedTreeHash": {"type": [S, "null"]}},
                              req=["path", "status", "timestamp", "approvedTreeHash"]))},
        req=["approvals"]),
    err_obj(),
), "审批表列表（可选 params.status 过滤 pending/approved/rejected）。")

# permission stored-decision（转发 permission 扩展，包 {success:true, data:<ext output>}）
PERM_OK = lambda data: obj({"success": {"const": True}, "data": data}, req=["success", "data"])
SPECS["permission_store_decision"] = ("approval",
                                      PERM_OK(obj({"status": {"const": "ok"}, "message": {"type": S}},
                                                  req=["status", "message"])),
                                      "存储权限决策（always allow）。⚠️ 外层 {success:true,data} 是 worker 包装，内层 data 才是 permission 扩展的返回。")

SPECS["permission_list_stored"] = ("approval",
                                   PERM_OK(obj({"rules": arr(RULE), "count": {"type": "integer"}},
                                               req=["rules", "count"])),
                                   "列出已存储决策规则（session + project 两级）。")

SPECS["permission_remove_stored"] = ("approval",
                                     PERM_OK(obj({"status": {"const": "ok"}, "removed": RULE},
                                                 req=["status", "removed"])),
                                     "删除某条存储决策（removed = 被删规则对象）。id 不存在 → 信封 success:false。")

SPECS["permission_clear_stored"] = ("approval",
                                    PERM_OK(obj({"status": {"const": "ok"}, "removed": {"type": "integer"}},
                                                req=["status", "removed"])),
                                    "清空所有存储决策（removed = 删除条数）。")

# ── group: file-snapshot ────────────────────────────────────────────
SPECS["restore_files"] = ("snapshot", one_of(
    obj({"restoredFiles": arr(obj({"path": {"type": S}, "action": {"type": S},
                                   "fromHash": {"type": [S, "null"]},
                                   "toHash": {"type": [S, "null"]},
                                   "reason": {"type": S}}, req=["path", "action", "reason"])),
         "restorePoint": {"type": [S, "null"]},
         "summary": obj({"restored": {"type": "integer"}, "deleted": {"type": "integer"},
                         "skipped": {"type": "integer"}},
                        req=["restored", "deleted", "skipped"])},
        req=["restoredFiles", "restorePoint", "summary"]),
    err_obj(),
), "代码回滚到指定 turn（tree-hash restore）。缺 toTurn → data.error；file-snapshot 未启用 → data.error。+ FilesRestored 事件。")

SPECS["turn_file_diff"] = ("snapshot", one_of(
    obj({"turnId": {"type": S}, "path": {"type": S}, "diff": {"type": S},
         "added": {"type": "integer"}, "removed": {"type": "integer"}},
        req=["turnId", "path", "diff", "added", "removed"]),
    err_obj(),
), "单轮单文件 diff。params.base: before（默认）/prev/disk。")

SPECS["turn_changes"] = ("snapshot", one_of(
    obj({"turnId": {"type": [S, "null"]},
         "files": arr(obj({"path": {"type": S},
                           "status": {"type": S, "enum": ["added", "modified", "deleted"]},
                           "added": {"type": "integer"}, "removed": {"type": "integer"}},
                          req=["path", "status", "added", "removed"])),
         "summary": obj({"files": {"type": "integer"}, "added": {"type": "integer"},
                         "removed": {"type": "integer"}},
                        req=["files", "added", "removed"])},
        req=["turnId", "files", "summary"]),
    err_obj(),
), "单轮变更文件列表。不传 turnId 时取最新 turn；无任何快照时 turnId=null 且 files=[]。")

SPECS["get_modified_files"] = ("snapshot", one_of(
    obj({"files": arr(obj({"path": {"type": S},
                           "status": {"type": S, "enum": ["added", "modified", "deleted", "unchanged"]},
                           "source": {"type": S, "enum": ["tool_write", "tool_edit", "turn_scan", "tool"]},
                           "turnId": {"type": S}, "toolCallId": {"type": S},
                           "tool": {"type": S}, "hasDiff": {"type": "boolean"}},
                          req=["path", "status", "source", "turnId", "toolCallId", "tool", "hasDiff"])),
         "summary": obj({"added": {"type": "integer"}, "modified": {"type": "integer"},
                         "deleted": {"type": "integer"}}, req=["added", "modified", "deleted"])},
        req=["files", "summary"]),
    err_obj(),
), "变更文件清单（可选 fromTurn/toTurn 范围）。file-snapshot 未启用 → data.error。")

SPECS["get_batch_diffs"] = ("snapshot", one_of(
    obj({"files": arr(obj({"path": {"type": S}, "diff": {"type": S},
                           "added": {"type": "integer"}, "removed": {"type": "integer"}},
                          req=["path", "diff", "added", "removed"])),
         "summary": obj({"files": {"type": "integer"}, "added": {"type": "integer"},
                         "removed": {"type": "integer"}}, req=["files", "added", "removed"])},
        req=["files", "summary"]),
    err_obj(),
), "批量 diff（可选 fromTurn/toTurn 范围，按 path 取首尾快照）。")

SPECS["get_file_history"] = ("snapshot", one_of(
    obj({"path": {"type": S},
         "history": arr(obj({"turnId": {"type": S}, "action": {"type": S, "enum": ["added", "deleted", "modified", "unchanged"]},
                             "toolCallId": {"type": S}, "tool": {"type": S},
                             "hash": {"type": [S, "null"]}},
                            req=["turnId", "action", "toolCallId", "tool", "hash"])),
         "count": {"type": "integer"}},
        req=["path", "history", "count"]),
    err_obj(),
), "单文件快照历史。缺 filePath → data.error。")

# ── group: extension / tool ─────────────────────────────────────────
SPECS["extension_rpc"] = ("extension",
    # data 的并集就是 any：worker 形状 ⊂ any，manager(singleton) 形状 = any。
    # 用裸 {} 描述 union（不能用 oneOf [worker, any]——空 schema 恒匹配会让 oneOf 对 worker 形状自相矛盾）。
    {},
    "调扩展私有 RPC。⚠️ 双路径形状分裂（data 为任意 JSON，两种形态都合法）：worker 路径（--session，内核/WASM 扩展）data={method, output}，output 为扩展返回值；manager 路径（singleton 扩展如 memory，无 session 直调）data=扩展返回值本身。plan 工具面也经此通道（extension='plan'）。测试断言见 rpc_schema_config_test.rs。")

SPECS["call_tool"] = ("extension",
                      obj({"tool": {"type": S}, "output": {"type": S}}, req=["tool", "output"]),
                      "直接调 LLM 注册的工具（绕过 LLM）。⚠️ data.output 是工具返回 JSON 的转义字符串（AgentResult<String>），不是对象。缺 tool → 信封 success:false。")

SPECS["extension_list"] = ("extension",
                           obj({"extensions": arr(obj({"path": {"type": S},
                                                       "abi_version": {"type": "integer"},
                                                       "tools": arr({"type": S})},
                                                      req=["path", "abi_version", "tools"]))},
                               req=["extensions"]),
                           "列出已加载的 WASM 扩展（.wasm 文件级，区别于 get_extensions 的内核扩展名）。")

SPECS["extension_add"] = ("extension",
                          obj({"tools": arr({"type": S})}, req=["tools"]),
                          "加载 .wasm 扩展并注册其工具。路径不能 canonicalize 或 load 失败 → 信封 success:false。+ ExtensionListChanged 事件。")

SPECS["extension_remove"] = ("extension",
                             obj({"removed_tools": arr({"type": S})}, req=["removed_tools"]),
                             "卸载 .wasm 扩展并反注册工具。+ ExtensionListChanged 事件。")

SPECS["extension_reload"] = ("extension",
                             obj({"tools": arr({"type": S})}, req=["tools"]),
                             "热重载 .wasm 扩展（先卸旧再加新）。")

SPECS["get_extensions"] = ("extension",
                           obj({"extensions": arr(obj({"name": {"type": S}}, req=["name"])),
                                "count": {"type": "integer"}},
                               req=["extensions", "count"]),
                           "列出已加载的内核扩展名（ExtensionRunner）。")

# anyOf 而非 oneOf：{extension, flags} 实例同时也匹配裸对象分支（FULL 型并集语义）
SPECS["get_flags"] = ("extension",
                      {"anyOf": [obj({"extension": {"type": S}, "flags": {"type": "object"}},
                                     req=["extension", "flags"]), {"type": "object"}]},
                      "读扩展 flag。无 params.extension → 所有扩展的 flag map（裸对象）；带 extension → {extension, flags}。")

SPECS["set_flag"] = ("extension", one_of(
    obj({"extension": {"type": S}, "flag": {"type": S}, "value": {},
         "set": {"const": True}}, req=["extension", "flag", "value", "set"]),
    err_obj(),
), "写扩展 flag。缺 extension/flag → data.error。")

SPECS["get_active_tools"] = ("extension",
                             obj({"tools": arr({"type": S}), "count": {"type": "integer"}},
                                 req=["tools", "count"]),
                             "列出当前激活工具名。")

SPECS["set_active_tools"] = ("extension",
                             obj({"activeTools": arr({"type": S}), "count": {"type": "integer"}},
                                 req=["activeTools", "count"]),
                             "限制激活工具集（restrict_tools）。+ SettingsChanged 事件。")

SPECS["get_tools"] = ("extension",
                      obj({"tools": arr(obj({"name": {"type": S}}, req=["name"]))}, req=["tools"]),
                      "⚠️ 硬编码 stub（9 个固定工具名），非真实工具表。真实表用 get_active_tools。")

SPECS["get_all_tools"] = ("extension", arr({}),
                          "⚠️ 硬编码 stub：恒返回空数组。")

SPECS["get_flag_values"] = ("extension", {"type": "object"},
                            "⚠️ 硬编码 stub：恒返回空对象。真实值用 get_flags。")

SPECS["get_skills"] = ("extension",
                       obj({"skills": arr(obj({"name": {"type": S},
                                               "source": {"type": S, "enum": ["global", "project"]},
                                               "path": {"type": S}, "brief": {"type": S}},
                                              req=["name", "source", "path", "brief"])),
                            "count": {"type": "integer"}},
                           req=["skills", "count"]),
                       "列出全局 + 项目级 skills（.md，brief 取前 3 行截 80 字符）。")

# ── group: mcp ──────────────────────────────────────────────────────
SPECS["get_mcp_servers"] = ("mcp", arr(SERVER),
                            "列出 MCP server 状态（方案 C：worker 经 manager_bridge 转发 host McpManager.server_list_json，data 是裸数组）。无 host 连接时走信封 error（success:false）。")

NULLABLE = lambda shape: {"oneOf": [shape, {"type": "null"}]}
SPECS["mcp_toggle_server"] = ("mcp", NULLABLE(obj({"name": {"type": S}, "enabled": {"type": "boolean"}},
                                                  req=["name", "enabled"])),
                              "启停 MCP server（转发 host）。缺 name/enabled → 信封 error。⚠️ data 形状来自 host resp.data，理论上可为 null。+ McpServerStatusChanged 事件。")

SPECS["mcp_restart_server"] = ("mcp", NULLABLE(obj({"name": {"type": S}, "status": {"type": S}},
                                                   req=["name", "status"])),
                               "重启 MCP server（转发 host，成功 status='connected'）。缺 name → 信封 error。")

SPECS["mcp_reload"] = ("mcp", NULLABLE(obj({"servers_loaded": {"type": "integer"},
                                            "connected": {"type": "integer"}},
                                           req=["servers_loaded", "connected"])),
                       "热重载 MCP 配置（转发 host，重读 config.json mcp_servers）。")

SPECS["mcp_read_resource"] = ("mcp", NULLABLE(obj({"content": {"type": S}}, req=["content"])),
                              "读 MCP 资源文本内容（转发 host，30s 超时）。缺 server/uri → 信封 error。")


def write_json(path, data):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write("\n")


def main():
    os.makedirs(OUT, exist_ok=True)
    index_entries = []
    for cmd, (group, data_schema, desc, *rest) in SPECS.items():
        command_const = rest[0].get("command_const") if rest and isinstance(rest[0], dict) else None
        schema = envelope(cmd, data_schema, desc, command_const)
        write_json(os.path.join(OUT, f"{cmd}.json"), schema)
        index_entries.append({
            "name": cmd,
            "file": f"{cmd}.json",
            "group": group,
            "description": desc.split("。")[0] + "。",
            "dynamicTested": False,  # filled below
        })

    dynamic = [
        "get_settings", "set_settings", "set_model", "cycle_thinking_level",
        "set_thinking_level", "get_available_models", "get_tier_models",
        "set_tier_models", "get_extensions", "extension_list", "get_flags",
        "set_flag", "get_active_tools", "set_active_tools", "get_skills",
        "list_dirs", "add_dir", "remove_dir", "set_permission_mode",
        "permission_store_decision", "permission_list_stored",
        "permission_remove_stored", "permission_clear_stored", "extension_rpc",
        "call_tool", "review_pending", "review_file_diff", "review_approve",
        "review_approvals", "review_reject", "get_modified_files",
        "turn_changes", "turn_file_diff", "restore_files", "get_mcp_servers",
        "mcp_reload", "set_cwd", "set_auto_compaction", "set_auto_retry",
        "review_approve_all", "review_reject_all",
    ]
    by_name = {e["name"]: e for e in index_entries}
    for name in dynamic:
        by_name[name]["dynamicTested"] = True

    index = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "schemas/rpc/config/_index.json",
        "domain": "config",
        "title": "ION RPC schema — 配置/审批/快照/扩展/MCP 域",
        "description": (
            "S4 批次：把 ION 动态 JSON RPC 的响应契约固化为 JSON Schema (draft 2020-12)。"
            "每个文件自包含（无外部 $ref），校验对象是完整响应信封 "
            "{id,type,command,success,data|error}。以 src/worker_rpc.rs 分发 match 的实际实现为准提取，"
            "历史怪癖（如 set_model 响应 command='get_state'、call_tool 的 output 为转义字符串、"
            "extension_rpc 双路径形状分裂、get_settings 脱敏形态）均已如实编码。"
        ),
        "conventions": {
            "envelope": ENVELOPE_NOTE,
            "selfContained": "每个 schema 无外部 $ref，可独立用 jsonschema 校验器加载",
            "oneOf": "success=true 分支校验 data；success=false 分支校验 error",
            "redaction": "get_settings 的密钥位（api_key/provider_api_keys/env/headers 等）schema 强制为 '***' 或 null（对齐 redact_json_secrets 三层规则）",
        },
        "quirks": [
            "set_model 响应信封 command='get_state'（历史怪癖）",
            "call_tool 成功时 data.output 是工具返回 JSON 的转义字符串（AgentResult<String>）",
            "extension_rpc 双路径形状分裂：worker=data{method,output}；manager(singleton)=扩展返回值直接作为 data",
            "get_settings 全量响应是脱敏后的形态（redacted_value），密钥位一律 '***'/null",
            "get_available_models / get_tier_models / get_mcp_servers 的 data 是裸数组/裸 map，不包对象壳",
            "review_pending 只回 diffStat 摘要不回内容；完整 diff 走 review_file_diff",
            "permission_* 响应外层是 worker 包装 {success,data}，内层 data 才是 permission 扩展返回",
            "get_tools / get_all_tools / get_flag_values 是硬编码 stub，真实数据用 get_active_tools / get_flags",
            "mcp_* 的 data 来自 host resp.data 转发，schema 允许 object|null",
        ],
        "groups": ["config", "approval", "snapshot", "extension", "mcp"],
        "commandCount": len(index_entries),
        "commands": index_entries,
    }
    write_json(os.path.join(OUT, "_index.json"), index)
    print(f"wrote {len(index_entries)} command schemas + _index.json to {OUT}")


if __name__ == "__main__":
    main()
