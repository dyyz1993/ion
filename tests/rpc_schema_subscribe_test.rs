//! S3 域（订阅/事件流/host 协议）JSON Schema 契约一致性测试。
//!
//! 两层验证：
//! 1. 静态：schemas/rpc/subscribe/**.json 全部可作为 draft 2020-12 编译；
//!    固化样例（正/负）逐一对上形状；_index.json 清单与磁盘文件互查。
//! 2. 动态：隔离 host（私有 HOME/ION_HOST_SOCKET/ION_SESSION_DIR，绝不碰真实 ~/.ion）
//!    raw socket 走真实协议：hello → subscribe → snapshot 帧 → 事件帧 → stale_route →
//!    ui_respond 同源 → host 级查询命令，每帧对 schema 验证。
//!
//! 契约来源：docs/design/SUBSCRIBE_PROTOCOL.md + src/bin/ion.rs（SessionRouter/accept 循环）
//! + src/worker_rpc.rs（StreamingExtension/emit_rpc_response_event）+ src/worker_registry.rs。

use jsonschema::Validator;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 静态层 — schema 编译 + 清单互查 + 固化样例验证
// ---------------------------------------------------------------------------

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/rpc/subscribe")
}

fn read_schema(rel: &str) -> Value {
    let p = schema_dir().join(rel);
    let raw = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

fn compile(rel: &str) -> Validator {
    let schema = read_schema(rel);
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|e| panic!("schema {} 不合法: {e}", rel))
}

/// 命令文件是「request/response 多变体容器」：验证具体帧必须取对应子 schema。
fn sub_validator(rel: &str, prop: &str) -> Validator {
    let schema = read_schema(rel);
    jsonschema::validator_for(&schema["properties"][prop])
        .unwrap_or_else(|e| panic!("subschema {}.{} 不合法: {e}", rel, prop))
}

/// 契约 A：目录下所有 .json 都必须能编译为 draft 2020-12 schema，且标注 $schema。
#[test]
fn all_schema_files_compile_as_draft_2020_12() {
    let mut found = 0;
    let mut stack = vec![schema_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read schema dir") {
            let p = entry.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&p).expect("read schema");
            let v: Value = serde_json::from_str(&raw).expect("schema is valid JSON");
            assert_eq!(
                v.get("$schema").and_then(|s| s.as_str()),
                Some("https://json-schema.org/draft/2020-12/schema"),
                "{} 必须声明 draft 2020-12",
                p.display()
            );
            jsonschema::validator_for(&v)
                .unwrap_or_else(|e| panic!("{} 编译失败: {e}", p.display()));
            found += 1;
        }
    }
    assert!(
        found >= 35,
        "S3 域应有 35+ 个 schema 文件，实际只有 {found}"
    );
}

/// 契约 B：_index.json 的 commands/frames 清单必须与磁盘文件一一对应（防清单漂移）。
#[test]
fn index_manifest_matches_files_on_disk() {
    let idx = read_schema("_index.json");
    let cmds: Vec<&str> = idx["properties"]["commands"]["items"]["enum"]
        .as_array()
        .expect("commands enum")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();
    let frames: Vec<&str> = idx["properties"]["frames"]["items"]["enum"]
        .as_array()
        .expect("frames enum")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();

    for c in &cmds {
        assert!(
            schema_dir().join(c).is_file(),
            "_index 列出的命令 {c} 在磁盘上不存在"
        );
    }
    for f in &frames {
        assert!(
            schema_dir().join("events").join(f).is_file(),
            "_index 列出的帧 {f} 在磁盘上不存在"
        );
    }
    // 反向：命令文件 = 8 个 command schema + 信封 + 索引（不含 events/）
    let top: Vec<_> = std::fs::read_dir(schema_dir())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".json") && n.as_str() != "_index.json")
        .collect();
    assert_eq!(
        top.len(),
        cmds.len() + 1,
        "顶层命令文件数（含 _response_envelope.json）应与 _index.commands 一致: {top:?}"
    );
    let events: Vec<_> = std::fs::read_dir(schema_dir().join("events"))
        .expect("read events dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".json"))
        .collect();
    assert_eq!(
        events.len(),
        frames.len(),
        "events/ 文件数应与 _index.frames 一致: {events:?}"
    );
}

/// 契约 C：event.type 枚举全集 = 19 种（穷举自源码产生点）。
#[test]
fn inner_event_type_enum_is_exhaustive() {
    let worker_event = read_schema("events/worker_event.json");
    let instance_event = read_schema("events/instance_event.json");
    let we: Vec<&str> = worker_event["properties"]["type"]["enum"]
        .as_array()
        .expect("worker_event enum")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();
    let ie: Vec<&str> = instance_event["properties"]["event"]["properties"]["type"]["enum"]
        .as_array()
        .expect("instance_event enum")
        .iter()
        .map(|v| v.as_str().expect("str"))
        .collect();
    assert_eq!(we.len(), 19, "worker_event event.type 全集应有 19 种: {we:?}");
    assert_eq!(
        we, ie,
        "worker_event 与 instance_event 的 event.type 枚举必须一致"
    );
    // 源码穷举核对（与 _index.json innerEvents 同源）
    let must_contain = [
        "text_delta",
        "agent_start",
        "agent_end",
        "agent_stopped",
        "message_start",
        "message_end",
        "tool_call_delta",
        "tool_call",
        "tool_execution_start",
        "tool_execution_update",
        "tool_execution_end",
        "auto_retry_start",
        "auto_retry_end",
        "error",
        "worker_ready",
        "extension_event",
        "custom",
        "child_crashed",
        "rpc_response",
    ];
    for t in must_contain {
        assert!(we.contains(&t), "event.type 枚举缺 {t}");
    }
}

// ── 固化样例（从源码形状手工转录） ──

fn v(s: &str) -> Value {
    serde_json::from_str(s).expect("fixture json")
}

#[test]
fn hello_request_and_response_validate() {
    let schema = read_schema("hello.json");
    let req = &schema["properties"]["request"];
    let resp = &schema["properties"]["response"];
    let vreq = json!({"id":"h1","method":"hello"});
    let vresp = json!({"type":"response","id":"h1","success":true,"data":{"protocolVersion":1}});
    assert!(Validator::new(req).expect("c").is_valid(&vreq));
    assert!(Validator::new(resp).expect("c").is_valid(&vresp));
    // 负例：版本错 / method 错
    assert!(!Validator::new(resp).expect("c").is_valid(&json!({"type":"response","id":"h1","success":true,"data":{"protocolVersion":2}})));
    assert!(!Validator::new(req).expect("c").is_valid(&json!({"id":"h1","method":"hello2"})));
}

#[test]
fn subscribe_request_three_modes_validate() {
    let schema = read_schema("subscribe.json");
    let c = Validator::new(&schema["properties"]["request"]).expect("compile");
    // instance（session；extension 缺省或空串）— host 分派第一优先
    assert!(c.is_valid(&json!({"id":"s1","method":"subscribe","session":"sess_x","replay":3})));
    assert!(c.is_valid(&json!({"method":"subscribe","session":"sess_x"})));
    assert!(c.is_valid(&json!({"method":"subscribe","session":"sess_x","extension":""})));
    // ui
    assert!(c.is_valid(&json!({"id":"u2","method":"subscribe","ui":true})));
    // extension 非空时即使带 ui:true 也走 ui 流（分派序：instance 条件不成立）
    assert!(c.is_valid(&json!({"method":"subscribe","session":"s","ui":true,"extension":"mem"})));
    // extension（带 session 过滤）/ all（无任何过滤）
    assert!(c.is_valid(&json!({"method":"subscribe","extension":"memory"})));
    assert!(c.is_valid(&json!({"method":"subscribe","extension":"memory","session":"sess_x"})));
    assert!(c.is_valid(&json!({"id":"s9","method":"subscribe"})));
    // 负例：类型错（session+非空 extension 的组合是合法的 extension 模式请求）
    assert!(!c.is_valid(&json!({"method":"subscribe","ui":"yes"})));
    assert!(!c.is_valid(&json!({"method":"subscribe","extension":123})));
    assert!(!c.is_valid(&json!({"method":"subscribe","session":123})));
}

#[test]
fn connection_error_frames_validate() {
    // 连接级 error 帧（subscribe 失败 + P0.1 行长超限拒绝帧）
    let schema = read_schema("subscribe.json");
    let c = Validator::new(&schema["properties"]["errors"]).expect("compile");
    // instance 订阅失败（60s 无 worker）
    assert!(c.is_valid(&json!({"type":"error","error":"no worker for session within 60s"})));
    // P0.1 行长超限帧（ion-protocol::line_too_large_frame）：带 limitBytes/actualBytes
    assert!(c.is_valid(&json!({
        "type":"error",
        "error":"line too large: 16785408 bytes exceeds limit 16777216",
        "limitBytes":16777216,
        "actualBytes":16785408
    })));
    // 负例：缺 type / limitBytes 非 16MiB / actualBytes 未超限
    assert!(!c.is_valid(&json!({"error":"boom"})));
    assert!(!c.is_valid(&json!({"type":"error","error":"x","limitBytes":1024})));
    assert!(!c.is_valid(&json!({
        "type":"error","error":"x","limitBytes":16777216,"actualBytes":16777216
    })));
}

#[test]
fn subscribed_ack_variants_validate() {
    let c = compile("events/subscribed_ack.json");
    // instance ack（带 epoch）— 源码：SessionRouter::handle_subscribe
    assert!(c.is_valid(&json!({"type":"subscribed","session":"sess_x","stream":"instance","epoch":1,"replayed":0})));
    assert!(c.is_valid(&json!({"type":"subscribed","session":"sess_x","stream":"instance","epoch":2,"replayed":3})));
    // ui ack — 源码：{"type":"subscribed","stream":"ui"}
    assert!(c.is_valid(&json!({"type":"subscribed","stream":"ui"})));
    // extension ack — 源码：{"type":"subscribed","extension":ext,"session":sid|null}
    assert!(c.is_valid(&json!({"type":"subscribed","extension":"memory","session":"sess_x"})));
    assert!(c.is_valid(&json!({"type":"subscribed","extension":"","session":null})));
    // 负例：instance ack 缺 epoch / epoch=0 / 缺 replayed
    assert!(!c.is_valid(&json!({"type":"subscribed","session":"s","stream":"instance","replayed":0})));
    assert!(!c.is_valid(&json!({"type":"subscribed","session":"s","stream":"instance","epoch":0,"replayed":0})));
    // 负例：unknown 帧
    assert!(!c.is_valid(&json!({"type":"subscribed"})));
}

#[test]
fn instance_event_frames_validate() {
    let c = compile("events/instance_event.json");
    // 实时帧（设计文档 1.1 示例）
    assert!(c.is_valid(&json!({"type":"instance_event","session":"sess_x","epoch":1,"event":{"type":"text_delta","delta":"..."}})));
    // replay 帧
    assert!(c.is_valid(&json!({"type":"instance_event","session":"sess_x","epoch":1,"replayed":true,"event":{"type":"worker_ready"}})));
    // 负例：缺 epoch / 非法内层事件类型
    assert!(!c.is_valid(&json!({"type":"instance_event","session":"sess_x","event":{"type":"text_delta"}})));
    assert!(!c.is_valid(&json!({"type":"instance_event","session":"sess_x","epoch":1,"event":{"type":"unknown_event_type"}})));
    assert!(!c.is_valid(&json!({"type":"stale_route","session":"sess_x","epoch":1})));
}

#[test]
fn snapshot_frame_validates_both_worker_states() {
    let c = compile("events/snapshot_frame.json");
    // 有 worker（设计文档 1.2 示例）
    let with_worker = v(r#"{
        "type":"instance_event","session":"sess_x","epoch":1,"snapshot":true,
        "event":{"type":"extension_event","extension":"host","customType":"snapshot",
                 "visibility":"ui_only","session":"sess_x",
                 "data":{"worker":{"workerId":"w-1","status":"Busy","model":"glm-5.2","agent":"build"},
                         "session":{"sessionId":"sess_x","model":"glm-5.2","provider":"zai","name":"demo"},
                         "pendingApprovals":{"count":2,"requests":["req-1","req-2"]},
                         "generatedAt":1757900000000}}}"#);
    assert!(c.is_valid(&with_worker));
    // 无 worker（worker=null，session 热字段缺索引时全 null）
    let null_worker = v(r#"{
        "type":"instance_event","session":"sess_y","epoch":1,"snapshot":true,
        "event":{"type":"extension_event","extension":"host","customType":"snapshot",
                 "visibility":"ui_only","session":"sess_y",
                 "data":{"worker":null,
                         "session":{"sessionId":"sess_y","model":null,"provider":null,"name":null},
                         "pendingApprovals":{"count":0,"requests":[]},
                         "generatedAt":1757900000001}}}"#);
    assert!(c.is_valid(&null_worker));
    // 负例：缺 generatedAt / pendingApprovals 缺 count / snapshot 未标 true / status 小写
    let bad = with_worker.clone();
    assert!(!c.is_valid(&drop_key(&bad, &["event", "data", "generatedAt"])));
    let bad2 = with_worker.clone();
    assert!(!c.is_valid(&drop_key(&bad2, &["event", "data", "pendingApprovals", "count"])));
    assert!(!c.is_valid(&drop_key(&with_worker, &["snapshot"])));
    let lower_status = set_at(&with_worker, &["event", "data", "worker", "status"], json!("busy"));
    assert!(!c.is_valid(&lower_status), "快照 worker.status 是 Display 大写，snake_case 必须拒绝");
}

#[test]
fn stale_route_frame_validates() {
    let c = compile("events/stale_route.json");
    assert!(c.is_valid(&json!({"type":"stale_route","customType":"stale_route","session":"sess_x","epoch":1,"currentEpoch":2})));
    // 负例：缺 currentEpoch / currentEpoch <= epoch 不可能（schema 层至少拦缺字段与类型）
    assert!(!c.is_valid(&json!({"type":"stale_route","customType":"stale_route","session":"sess_x","epoch":1})));
    assert!(!c.is_valid(&json!({"type":"stale_route","session":"sess_x","epoch":1,"currentEpoch":2})));
}

#[test]
fn rpc_response_event_validates() {
    let c = compile("events/rpc_response_event.json");
    assert!(c.is_valid(&json!({"type":"rpc_response","id":"r1","method":"set_model","success":true,"sessionId":"sess_x","timestamp":1757900000000i64})));
    assert!(c.is_valid(&json!({"type":"rpc_response","id":"r2","method":"prompt","success":false,"sessionId":"sess_x","timestamp":1757900000001i64,"error":"model not found"})));
    // 负例：缺 method / success 非布尔
    assert!(!c.is_valid(&json!({"type":"rpc_response","id":"r1","success":true,"sessionId":"s","timestamp":1})));
    assert!(!c.is_valid(&json!({"type":"rpc_response","id":"r1","method":"m","success":"yes","sessionId":"s","timestamp":1})));
}

#[test]
fn ui_and_extension_stream_frames_validate() {
    let cu = compile("events/ui_event_frame.json");
    assert!(cu.is_valid(&json!({"type":"ui_event","ui_type":"Ask","extension":"ui","session":null,"data":{"title":"q","message":"?"},"route":"ui"})));
    assert!(cu.is_valid(&json!({"type":"ui_event","ui_type":"session_created","extension":"session","session":"sess_x","data":{"sessionId":"sess_x"},"route":"ui"})));
    assert!(!cu.is_valid(&json!({"type":"ui_event","ui_type":"Ask","extension":"ui","session":null,"data":{},"route":"extension"})));

    let ce = compile("events/extension_event_frame.json");
    assert!(ce.is_valid(&json!({"type":"extension_event","extension":"memory","customType":"memory_saved","session":"sess_x","persisted":false,"visibility":"llm_and_ui","correlation_id":"","data":{}})));
    assert!(ce.is_valid(&json!({"type":"extension_event","extension":"todo","customType":"todo_updated","session":null,"persisted":true,"visibility":"ui_only","correlation_id":"c-1","data":{"n":1}})));
    // 负例：非法 visibility / 缺 persisted
    assert!(!ce.is_valid(&json!({"type":"extension_event","extension":"m","customType":"x","session":null,"persisted":false,"visibility":"both","correlation_id":"","data":{}})));
    assert!(!ce.is_valid(&json!({"type":"extension_event","extension":"m","customType":"x","session":null,"visibility":"ui_only","correlation_id":"","data":{}})));
}

#[test]
fn overview_shapes_validate() {
    let payload = json!({
        "workers": [{"worker_id":"w-1","session_id":"sess_x","project":"/tmp/p","status":"busy",
                     "exit_code":null,"exit_reason":null,"model":"glm-5.2","agent":"build",
                     "channels":[],"parent":null,"children":[],"latest_output":[],"log_short":null,
                     "model_size":128000,"started_at":1757900000000i64}],
        "projects": [{"name":"p","path":"/tmp/p","worker_count":1}],
        "total_workers": 1, "total_projects": 1, "total_stale": 0, "total_dead": 0,
        "sessions": [{"session_id":"sess_x","worker_id":"w-1","project":"/tmp/p","created_by":null}]
    });
    let ov = compile("events/overview.json");
    assert!(ov.is_valid(&payload));
    // 坑：get_overview 的 status 是 serde snake_case；大写必须拒绝
    let mut bad = payload.clone();
    bad["workers"][0]["status"] = json!("Busy");
    assert!(!ov.is_valid(&bad));

    let cr = compile("get_overview.json");
    assert!(cr.is_valid(&json!({"type":"response","id":"g1","success":true,"data":payload})));

    let cs = compile("events/overview_snapshot.json");
    assert!(cs.is_valid(&json!({"type":"overview_snapshot","data":payload})));
    assert!(!cs.is_valid(&json!({"type":"overview_snapshot"})));

    // list_sessions 的 status 是 Display 大写（取 response 子 schema 验证）
    let cl = sub_validator("list_sessions.json", "response");
    assert!(cl.is_valid(&json!({"type":"response","id":"l1","success":true,"data":{"sessions":[
        {"session_id":"sess_x","agent":"build","status":"Busy","model":"glm-5.2",
         "started_at":1757900000000i64,"latest_output":["hi"],"log_short":"hi","model_size":128000}]}})));
    let mut bad_ls = v(r#"{"type":"response","id":"l1","success":true,"data":{"sessions":[
        {"session_id":"sess_x","agent":"build","status":"busy","model":"m",
         "started_at":1757900000000,"latest_output":[],"log_short":null,"model_size":null}]}}"#);
    assert!(!cl.is_valid(&bad_ls), "list_sessions status 大小写是两套序列化的真实差异");
    bad_ls["data"]["sessions"][0]["status"] = json!("Idle");
    assert!(cl.is_valid(&bad_ls));
}

#[test]
fn ui_respond_and_verb_schemas_validate() {
    // 命令文件是多变体容器：请求/各响应变体分别取子 schema 验证
    let c_req = sub_validator("ui_respond.json", "request");
    let c_ok = sub_validator("ui_respond.json", "responseSuccess");
    let c_rej = sub_validator("ui_respond.json", "responseRejected");
    let c_nf = sub_validator("ui_respond.json", "responseNotFound");
    assert!(c_req.is_valid(&json!({"id":"u1","method":"ui_respond","params":{"request_id":"r","response":"allow"}})));
    assert!(!c_req.is_valid(&json!({"id":"u1","method":"ui_respond"})));
    assert!(c_ok.is_valid(&json!({"type":"response","id":"u1","success":true,"data":{"request_id":"r","response":"allow"}})));
    assert!(c_rej.is_valid(&json!({"type":"response","id":"u1","success":false,"error":"ui_respond rejected: requires a prior subscribe {ui:true} on the same connection"})));
    assert!(c_nf.is_valid(&json!({"type":"response","id":"u1","success":false,"error":"request not found or already expired"})));
    // 负例：同源错误信息格式错 / not found 文案错 / success 形状错
    assert!(!c_rej.is_valid(&json!({"type":"response","id":"u1","success":false,"error":"rejected"})));
    assert!(!c_nf.is_valid(&json!({"type":"response","id":"u1","success":false,"error":"not found"})));
    assert!(!c_ok.is_valid(&json!({"type":"response","id":"u1","success":true,"data":{"request_id":"r"}})));
    assert!(!c_ok.is_valid(&json!({"type":"response","id":"u1","success":false,"data":{"request_id":"r","response":"allow"}})));

    let cv = sub_validator("verb_pending.json", "response");
    assert!(cv.is_valid(&json!({"type":"response","id":"v1","success":true,"data":{"pending":[
        {"requestId":"vr-1","session":"sess_x","verb":"write_file","args":{"path":"a.txt"},"ageMs":1200}]}})));
    assert!(cv.is_valid(&json!({"type":"response","id":"v1","success":true,"data":{"pending":[]}})));
    assert!(!cv.is_valid(&json!({"type":"response","id":"v1","success":true,"data":{"pending":[{"requestId":"x"}]}})));

    let cw_req = sub_validator("verb_review.json", "request");
    let cw_ok = sub_validator("verb_review.json", "responseSuccess");
    let cw_nf = sub_validator("verb_review.json", "responseNotFound");
    let cw_miss = sub_validator("verb_review.json", "responseMissingParam");
    assert!(cw_req.is_valid(&json!({"id":"v2","method":"verb_review","params":{"requestId":"vr-1","approve":true}})));
    assert!(cw_req.is_valid(&json!({"id":"v2","method":"verb_review","params":{"requestId":"vr-1"}})));
    assert!(!cw_req.is_valid(&json!({"id":"v2","method":"verb_review","params":{}})));
    assert!(cw_ok.is_valid(&json!({"type":"response","id":"v2","success":true,"data":{"requestId":"vr-1","approved":true}})));
    assert!(!cw_ok.is_valid(&json!({"type":"response","id":"v2","success":true,"data":{"requestId":"vr-1"}})));
    assert!(cw_nf.is_valid(&json!({"type":"response","id":"v2","success":false,"error":"verb approval not found: vr-1"})));
    assert!(!cw_nf.is_valid(&json!({"type":"response","id":"v2","success":false,"error":"other"})));
    assert!(cw_miss.is_valid(&json!({"type":"response","id":"v2","success":false,"error":"missing params.requestId"})));
}

#[test]
fn inner_typed_event_schemas_validate() {
    // 每个内层事件 schema 一个正例（形状转录自源码产生点）
    let cases: &[(&str, Value)] = &[
        ("events/text_delta.json", json!({"type":"text_delta","delta":"hi","sessionId":"s"})),
        // faux 快速路径无 sessionId
        ("events/text_delta.json", json!({"type":"text_delta","delta":"hi"})),
        ("events/agent_start.json", json!({"type":"agent_start","sessionId":"s","timestamp":1})),
        ("events/agent_end.json", json!({"type":"agent_end","sessionId":"s","willRetry":false,"messages":3,"timestamp":1})),
        ("events/agent_stopped.json", json!({"type":"agent_stopped","sessionId":"s","timestamp":1,"reason":"user_abort"})),
        ("events/message_start.json", json!({"type":"message_start","sessionId":"s","role":"assistant","content_length":10,"timestamp":1})),
        ("events/message_end.json", json!({"type":"message_end","sessionId":"s","role":"assistant","usage":{"input":10,"output":5,"total":15},"timestamp":1})),
        ("events/tool_call_delta.json", json!({"type":"tool_call_delta","sessionId":"s","delta":"{\"a\"","toolName":"read","timestamp":1})),
        ("events/tool_call.json", json!({"type":"tool_call","tool":"read"})),
        ("events/tool_execution_start.json", json!({"type":"tool_execution_start","sessionId":"s","toolCallId":"tc1","toolName":"read","args":{"path":"a"},"timestamp":1})),
        ("events/tool_execution_update.json", json!({"type":"tool_execution_update","sessionId":"s","toolCallId":"tc1","toolName":"bash","args":{},"partialResult":"compiling"})),
        ("events/tool_execution_end.json", json!({"type":"tool_execution_end","sessionId":"s","toolCallId":"tc1","toolName":"read","isError":false,"result":{"output":"x"},"durationMs":12,"timestamp":1})),
        ("events/auto_retry_start.json", json!({"type":"auto_retry_start","sessionId":"s","attempt":1,"maxRetries":3,"timestamp":1})),
        ("events/auto_retry_end.json", json!({"type":"auto_retry_end","sessionId":"s","success":false,"attempt":3,"timestamp":1})),
        ("events/error.json", json!({"type":"error","message":"boom","timestamp":1})),
        ("events/worker_ready.json", json!({"type":"worker_ready"})),
        ("events/extension_event.json", json!({"type":"extension_event","extension":"bash","customType":"proc_exited","visibility":"llm_and_ui","timestamp":1,"data":{"code":0}})),
        ("events/extension_event.json", json!({"type":"extension_event","extension":"host","customType":"snapshot","visibility":"ui_only","session":"s","data":{}})),
        ("events/custom.json", json!({"type":"custom","customType":"my_event","data":{"k":"v"}})),
        ("events/child_crashed.json", json!({"type":"child_crashed","session_id":"sess_child","exit_reason":"SIGKILL"})),
        ("events/rpc_response_event.json", json!({"type":"rpc_response","id":"r","method":"prompt","success":true,"sessionId":"s","timestamp":1})),
    ];
    for (rel, inst) in cases {
        let schema = compile(rel);
        assert!(schema.is_valid(inst), "{rel} 应接受 {inst}");
    }
    // 集中负例：各 schema 的硬约束
    let neg: &[(&str, Value)] = &[
        ("events/text_delta.json", json!({"type":"text_delta","delta":""})),
        ("events/text_delta.json", json!({"type":"text_delta"})),
        ("events/agent_stopped.json", json!({"type":"agent_stopped","sessionId":"s"})),
        ("events/message_end.json", json!({"type":"message_end","sessionId":"s","role":"a","usage":{"input":1,"output":1}})),
        ("events/tool_call_delta.json", json!({"type":"tool_call_delta","delta":"","toolName":"t"})),
        ("events/tool_execution_update.json", json!({"type":"tool_execution_update","sessionId":"s","toolCallId":"t","toolName":"n","args":{}})),
        ("events/auto_retry_start.json", json!({"type":"auto_retry_start","sessionId":"s","attempt":0,"maxRetries":3,"timestamp":1})),
        ("events/worker_ready.json", json!({"type":"worker_ready","extra":1})),
        ("events/custom.json", json!({"type":"custom","data":{}})),
        ("events/child_crashed.json", json!({"type":"child_crashed","sessionId":"s","exit_reason":"x"})),
    ];
    for (rel, inst) in neg {
        let schema = compile(rel);
        assert!(!schema.is_valid(inst), "{rel} 应拒绝 {inst}");
    }
}

#[test]
fn response_envelope_validates() {
    let c = compile("_response_envelope.json");
    assert!(c.is_valid(&json!({"type":"response","id":"1","success":true,"data":{}})));
    assert!(c.is_valid(&json!({"type":"response","id":null,"success":false,"error":"invalid JSON: x"})));
    assert!(c.is_valid(&json!({"type":"response","id":"1","success":true,"data":{},"session":"sess_x"})));
    assert!(!c.is_valid(&json!({"type":"error","error":"x"})));
    assert!(!c.is_valid(&json!({"type":"response","id":"1"})));
}

// ── 小工具 ──

fn drop_key(v: &Value, path: &[&str]) -> Value {
    let mut out = v.clone();
    let mut cur = &mut out;
    for k in &path[..path.len() - 1] {
        cur = cur.get_mut(*k).expect("path");
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.remove(path[path.len() - 1]);
    }
    out
}

fn set_at(v: &Value, path: &[&str], val: Value) -> Value {
    let mut out = v.clone();
    let mut cur = &mut out;
    for k in &path[..path.len() - 1] {
        cur = cur.get_mut(*k).expect("path");
    }
    cur[path[path.len() - 1]] = val;
    out
}

// ---------------------------------------------------------------------------
// 动态层 — 隔离 host + raw socket 全链路
// ---------------------------------------------------------------------------

/// 隔离 host 守卫：私有 HOME / ION_HOST_SOCKET / ION_SESSION_DIR，Drop 时精确 kill 自己的 PID。
struct HostGuard {
    child: Child,
    sock: PathBuf,
    dir: PathBuf,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl HostGuard {
    fn start() -> Self {
        let ion = PathBuf::from(env!("CARGO_BIN_EXE_ion"));
        let dir = std::env::temp_dir().join(format!(
            "ion-schema-sub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .subsec_nanos()
        ));
        let home = dir.join("home");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&proj).expect("mkdir proj");
        std::fs::write(proj.join("README.md"), "schema subscribe test\n").expect("write proj");
        let sock = dir.join("host.sock");
        let child = Command::new(&ion)
            .arg("serve")
            .env("HOME", &home)
            .env("ION_HOST_SOCKET", &sock)
            .env("ION_SESSION_DIR", dir.join("sessions"))
            .env("ION_FAUX_REPLY", "schema subscribe test ready")
            // 静态响应队列只有 1 条；REPEAT=1 让空队列时重复最后一条，避免后续 prompt 全进 auto_retry
            .env("ION_FAUX_REPEAT", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ion serve");
        let guard = HostGuard { child, sock, dir };
        // 等 socket 就绪
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if UnixStream::connect(&guard.sock).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "host socket 30s 未就绪: {}",
                guard.sock.display()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        guard
    }

    /// 一次性 RPC 连接：发一行，读至多 max_lines 行（host RPC 模式回包后关连接）。
    fn rpc(&self, line: &str, max_lines: usize, timeout: Duration) -> Vec<Value> {
        let mut stream = UnixStream::connect(&self.sock).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set timeout");
        use std::io::Write;
        stream.write_all(format!("{line}\n").as_bytes()).expect("send");
        read_lines_until(&mut stream, max_lines, Instant::now() + timeout)
    }
}

/// 带超时的逐行读取（跨 read timeout 重试直到 deadline / EOF / 读满）。
fn read_lines_until(stream: &mut UnixStream, max_lines: usize, deadline: Instant) -> Vec<Value> {
    let mut out = Vec::new();
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    loop {
        if out.len() >= max_lines || Instant::now() >= deadline {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                    out.push(v);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    out
}

/// 长连接订阅者：持续收帧直到 deadline，全部解析为 Value。
struct SubConn {
    stream: UnixStream,
}

impl SubConn {
    fn connect(sock: &Path) -> Self {
        let stream = UnixStream::connect(sock).expect("subscribe connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        SubConn { stream }
    }
    fn send(&mut self, v: &Value) {
        use std::io::Write;
        self.stream
            .write_all(format!("{}\n", v).as_bytes())
            .expect("send");
    }
    fn drain(&mut self, deadline: Instant) -> Vec<Value> {
        read_lines_until(&mut self.stream, 200, deadline)
    }
    fn read_expecting(&mut self, n: usize, timeout: Duration) -> Vec<Value> {
        read_lines_until(&mut self.stream, n, Instant::now() + timeout)
    }
}

fn validate_or_panic(schema: &Validator, frame: &Value, label: &str) {
    if !schema.is_valid(frame) {
        let errs: Vec<String> = schema.iter_errors(frame).map(|e| e.to_string()).collect();
        panic!("{label} 帧 schema 验证失败: {errs:?}\n帧: {frame}");
    }
}

/// 动态全链路：hello → subscribe(快照/replay/事件) → stale_route → 同源 → host 查询。
/// （拆成多个 #[test] 会各起一个 host，串行共用一个 host 更快且状态可控。）
#[test]
fn live_host_protocol_frames_match_schemas() {
    let host = HostGuard::start();
    let ack_schema = compile("events/subscribed_ack.json");
    let snap_schema = compile("events/snapshot_frame.json");
    let inst_schema = compile("events/instance_event.json");

    // ── 1. hello 握手 ──
    let hello_frames = host.rpc(r#"{"id":"h1","method":"hello"}"#, 2, Duration::from_secs(5));
    assert_eq!(hello_frames.len(), 1, "hello 应恰回一帧");
    let hresp = &hello_frames[0];
    // hello 响应子形状：用 read_schema 取子 schema 编译
    validate_or_panic(
        &jsonschema::validator_for(&read_schema("hello.json")["properties"]["response"])
            .expect("c"),
        hresp,
        "hello.response",
    );

    // ── 2. create_worker（faux）拿 session ──
    let proj = host.dir.join("proj");
    let create = host.rpc(
        &format!(
            r#"{{"id":"c1","method":"create_worker","params":{{"relation":"child","creator":"schema-ci","project_path":"{}","initial_prompt":"schema ci"}}}}"#,
            proj.display()
        ),
        2,
        Duration::from_secs(30),
    );
    assert_eq!(create.len(), 1, "create_worker 回包: {create:?}");
    let sid = create[0]["data"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let wid = create[0]["data"]["workerId"]
        .as_str()
        .expect("workerId")
        .to_string();

    // ── 3. subscribe（replay=3）→ ack(epoch) → snapshot → 实时事件 ──
    // 先跑一轮 faux prompt 产生可回放历史
    let _ = host.rpc(
        &format!(r#"{{"id":"p0","method":"prompt","session":"{sid}","params":{{"text":"warmup"}}}}"#),
        2,
        Duration::from_secs(20),
    );
    std::thread::sleep(Duration::from_millis(800));

    let mut sub = SubConn::connect(&host.sock);
    sub.send(&json!({"id":"s1","method":"subscribe","session":sid,"replay":3}));
    let first = sub.read_expecting(2, Duration::from_secs(15));
    assert!(first.len() >= 2, "应收到 ack+snapshot，实际 {} 帧", first.len());
    validate_or_panic(&ack_schema, &first[0], "subscribed ack");
    assert_eq!(first[0]["epoch"], 1, "首次订阅 epoch=1");
    assert_eq!(first[0]["session"], json!(sid));
    validate_or_panic(&snap_schema, &first[1], "snapshot frame");
    assert_eq!(first[1]["epoch"], 1, "快照帧带 epoch=1");

    // 触发事件流：另一连接 prompt → text_delta/agent_start/agent_end + rpc_response
    let prompt_handle = {
        let sock = host.sock.clone();
        let line = format!(r#"{{"id":"p1","method":"prompt","session":"{sid}","params":{{"text":"stream events"}}}}"#);
        std::thread::spawn(move || {
            let mut s = UnixStream::connect(&sock).expect("prompt conn");
            use std::io::Write;
            s.write_all(format!("{line}\n").as_bytes()).expect("send prompt");
            read_lines_until(&mut s, 2, Instant::now() + Duration::from_secs(20))
        })
    };

    let frames = sub.drain(Instant::now() + Duration::from_secs(20));
    let _ = prompt_handle.join().expect("prompt thread");
    assert!(
        frames.len() >= 2,
        "prompt 后应收到事件帧，实际 {} 帧",
        frames.len()
    );
    let mut saw_text_delta = false;
    let mut saw_agent_start = false;
    let mut saw_agent_end = false;
    let mut saw_rpc_response = false;
    for f in &frames {
        validate_or_panic(&inst_schema, f, "instance_event");
        assert_eq!(f["session"], json!(sid), "所有帧 session 一致");
        assert_eq!(f["epoch"], 1, "事件帧 epoch 恒为 1（未重派）");
        let et = f["event"]["type"].as_str().expect("event.type");
        match et {
            "text_delta" => {
                saw_text_delta = true;
                validate_or_panic(&compile("events/text_delta.json"), &f["event"], "text_delta");
            }
            "agent_start" => {
                saw_agent_start = true;
                validate_or_panic(&compile("events/agent_start.json"), &f["event"], "agent_start");
            }
            "agent_end" => {
                saw_agent_end = true;
                validate_or_panic(&compile("events/agent_end.json"), &f["event"], "agent_end");
            }
            "rpc_response" => {
                saw_rpc_response = true;
                validate_or_panic(
                    &compile("events/rpc_response_event.json"),
                    &f["event"],
                    "rpc_response",
                );
            }
            _ => {}
        }
    }
    assert!(saw_text_delta, "faux prompt 应产生 text_delta（帧: {frames:?}）");
    assert!(saw_agent_start, "应产生 agent_start");
    assert!(saw_agent_end, "应产生 agent_end");
    assert!(saw_rpc_response, "prompt RPC 应广播 rpc_response 摘要事件");

    // ── 4. epoch 栅栏：kill_worker → 旧订阅收 stale_route（最后一帧）→ 重派后 epoch=2 ──
    let stale_schema = compile("events/stale_route.json");
    let mut g2 = SubConn::connect(&host.sock);
    g2.send(&json!({"id":"g2","method":"subscribe","session":sid}));
    let g2_head = g2.read_expecting(2, Duration::from_secs(15));
    assert!(g2_head.len() >= 2, "G2 订阅 ack+snapshot: {g2_head:?}");
    validate_or_panic(&ack_schema, &g2_head[0], "G2 ack");
    validate_or_panic(&snap_schema, &g2_head[1], "G2 snapshot");

    let kill = host.rpc(
        &json!({"id":"k1","method":"kill_worker","params":{"workerId":wid}}).to_string(),
        2,
        Duration::from_secs(10),
    );
    assert_eq!(kill[0]["success"], json!(true), "kill_worker: {kill:?}");

    let stale_frames = g2.drain(Instant::now() + Duration::from_secs(15));
    assert!(
        !stale_frames.is_empty(),
        "kill 后旧订阅应收 stale_route"
    );
    let last = stale_frames.last().expect("last frame");
    validate_or_panic(&stale_schema, last, "stale_route");
    assert_eq!(last["epoch"], 1, "旧订阅 epoch=1");
    assert_eq!(last["currentEpoch"], 2, "推进后 currentEpoch=2");
    // 恰一条 stale_route 且是最后一帧；之前的帧只能是合法 instance_event
    assert_eq!(
        stale_frames.iter().filter(|f| f["type"] == json!("stale_route")).count(),
        1,
        "恰一条 stale_route"
    );
    for f in &stale_frames[..stale_frames.len() - 1] {
        validate_or_panic(&inst_schema, f, "stale_route 前的帧");
    }

    // 重派：同 session prompt（auto-create 拉新 worker）→ 新订阅 epoch=2
    let _ = host.rpc(
        &format!(r#"{{"id":"p2","method":"prompt","session":"{sid}","params":{{"text":"respawn"}}}}"#),
        2,
        Duration::from_secs(20),
    );
    std::thread::sleep(Duration::from_millis(800));
    let mut sub3 = SubConn::connect(&host.sock);
    sub3.send(&json!({"id":"s3","method":"subscribe","session":sid}));
    let head3 = sub3.read_expecting(2, Duration::from_secs(15));
    assert!(head3.len() >= 2, "重派后订阅: {head3:?}");
    validate_or_panic(&ack_schema, &head3[0], "respawn ack");
    assert_eq!(head3[0]["epoch"], 2, "重派后新订阅 epoch=2");
    validate_or_panic(&snap_schema, &head3[1], "respawn snapshot");
    assert_eq!(head3[1]["epoch"], 2);

    // ── 5. ui_respond 同源绑定 ──
    let c_rej = sub_validator("ui_respond.json", "responseRejected");
    let c_nf = sub_validator("ui_respond.json", "responseNotFound");
    // 5a 未订阅连接 → rejected（同源错误信息）
    let deny = host.rpc(
        r#"{"id":"u1","method":"ui_respond","params":{"request_id":"nonexistent","response":"allow"}}"#,
        2,
        Duration::from_secs(5),
    );
    assert_eq!(deny.len(), 1);
    assert!(
        c_rej.is_valid(&deny[0]),
        "ui_respond rejected 帧应对 responseRejected 子 schema: {deny:?}"
    );
    let err = deny[0]["error"].as_str().unwrap_or_default();
    assert!(
        err.starts_with("ui_respond rejected: "),
        "拒绝错误信息应以 'ui_respond rejected: ' 开头: {err}"
    );

    // 5b 同连接 subscribe ui:true → ui_respond（unknown id）→ not found（过同源闸）
    let mut g4 = SubConn::connect(&host.sock);
    g4.send(&json!({"id":"u2","method":"subscribe","ui":true}));
    let ack_ui = g4.read_expecting(1, Duration::from_secs(5));
    assert_eq!(ack_ui.len(), 1);
    validate_or_panic(&ack_schema, &ack_ui[0], "ui subscribe ack");
    g4.send(&json!({"id":"u3","method":"ui_respond","params":{"request_id":"nonexistent","response":"allow"}}));
    let resp_ui = g4.read_expecting(1, Duration::from_secs(5));
    assert_eq!(resp_ui.len(), 1, "同源 ui_respond 应有回包");
    assert!(
        c_nf.is_valid(&resp_ui[0]),
        "同源 ui_respond 帧应对 responseNotFound 子 schema: {resp_ui:?}"
    );
    assert_eq!(
        resp_ui[0]["error"],
        json!("request not found or already expired"),
        "过同源闸后落到业务层 not found"
    );

    // ── 6. host 级查询命令 ──
    let ls = host.rpc(r#"{"id":"l1","method":"list_sessions"}"#, 2, Duration::from_secs(5));
    validate_or_panic(&sub_validator("list_sessions.json", "response"), &ls[0], "list_sessions");
    assert!(
        !ls[0]["data"]["sessions"].as_array().expect("sessions").is_empty(),
        "至少有一个运行中 session"
    );

    let go = host.rpc(r#"{"id":"g9","method":"get_overview"}"#, 2, Duration::from_secs(5));
    validate_or_panic(
        &jsonschema::validator_for(&read_schema("get_overview.json")["properties"]["response"])
            .expect("c"),
        &go[0],
        "get_overview",
    );
    // overview 载荷独立 schema 也要过
    validate_or_panic(&compile("events/overview.json"), &go[0]["data"], "overview payload");

    let vp = host.rpc(r#"{"id":"v1","method":"verb_pending"}"#, 2, Duration::from_secs(5));
    validate_or_panic(&sub_validator("verb_pending.json", "response"), &vp[0], "verb_pending");
    let vr = host.rpc(
        r#"{"id":"v2","method":"verb_review","params":{"requestId":"nonexistent","approve":false}}"#,
        2,
        Duration::from_secs(5),
    );
    validate_or_panic(&sub_validator("verb_review.json", "responseNotFound"), &vr[0], "verb_review not found");
    assert!(
        vr[0]["error"].as_str().map(|e| e.starts_with("verb approval not found: ")).unwrap_or(false),
        "verb_review 未命中错误信息"
    );

    // ── 7. subscribe_overview：ack(initial) + 一帧 overview_snapshot ──
    let cso = sub_validator("subscribe_overview.json", "ack");
    let mut so = SubConn::connect(&host.sock);
    so.send(&json!({"id":"o1","method":"subscribe_overview"}));
    let so_ack = so.read_expecting(1, Duration::from_secs(5));
    assert_eq!(so_ack.len(), 1, "subscribe_overview ack: {so_ack:?}");
    assert!(
        cso.is_valid(&so_ack[0]),
        "subscribe_overview ack 应对 ack 子 schema"
    );
    assert_eq!(so_ack[0]["data"]["stream"], json!("overview"));
    validate_or_panic(&compile("events/overview.json"), &so_ack[0]["data"]["initial"], "overview initial");
    // kill 一个 worker 触发 broadcast_overview → 收 overview_snapshot 帧
    let _ = host.rpc(
        &json!({"id":"k2","method":"kill_worker","params":{"workerId":current_worker_of(&host, &sid)}}).to_string(),
        2,
        Duration::from_secs(10),
    );
    let so_frames = so.drain(Instant::now() + Duration::from_secs(10));
    assert!(
        so_frames.iter().any(|f| {
            f["type"] == json!("overview_snapshot")
                && compile("events/overview_snapshot.json").is_valid(f)
        }),
        "应收到合法 overview_snapshot 帧: {so_frames:?}"
    );
}

/// 在 kill 前查当前该 session 的 worker id（重派后 wid 已变）。
fn current_worker_of(host: &HostGuard, sid: &str) -> String {
    let ls = host.rpc(r#"{"id":"wq","method":"list_workers"}"#, 2, Duration::from_secs(5));
    for w in ls[0]["data"]["workers"].as_array().expect("workers") {
        if w["sessionId"] == json!(sid) {
            return w["workerId"].as_str().expect("workerId").to_string();
        }
    }
    panic!("session {sid} 无 worker 可 kill: {ls:?}");
}
