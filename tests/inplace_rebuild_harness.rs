//! AUTO-RECOVERY 原地重建 harness（K4：恢复保真度升级——同 sid 原地重建优先）。
//!
//! 背景：旧 AUTO-RECOVERY 重派 = 新 sid + 摘要接力（原始任务 300 字符 + 最近 6 轮），
//! 丢上下文细节。而 worker「同 sid 启动 → 从 JSONL 全历史预加载」的路径本就存在
//!（worker_rpc.rs：SessionFile::load → with_messages），只是重派没用它。
//!
//! K4 三档决策：①会话文件直落命中 → 原 sid 原地重建（全历史预加载 + 短引导）；
//! ②扫描命中回流副本 → 远端 respawn 也原地重建（预加载发生在远端自己的副本）；
//! ③都没有/空文件 → 降级摘要接力（现状）或干脆不重派（现状）。
//!
//! 验收标准（对齐「排队回执 ≠ 送达」教训）：断言的是重建后新轮的**真实作答**——
//! FauxProvider Factory 检查 LLM 上下文里是否真的出现了（a）中断前的完整历史
//!（原始任务 + 中断前回复）与（b）crash 前排队的用户消息，再分支应答。
//!
//! 场景：
//!   T1 原地重建全链路：agent 跑到一半（任务+回复已落盘）→ 排队消息落盘 → crash →
//!      真实 try_auto_respawn 决策 = 原地重建（原 sid）→ 按 fork-child 语义重建
//!     （全历史预加载 + queued_input 回放）→ 注入短引导开新轮 → 工厂断言全历史 + 排队消息都进了上下文。
//!   T2 降级链路：无文件 → 不重派（None）；header-only → 不重派（None）——现状行为不变。
//!   T3 回流扫描 + 本机 respawn 降级接力：新 sid 空白重建 → 摘要提示词携带原始任务 →
//!      工厂断言任务文本经提示词进入上下文（接力保真度下限）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::messages::{ContentBlock, Message, TextContent, UserMessage};
use ion::worker_registry::{WorkerRecord, WorkerStatus};
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::{AssistantContentBlock, MessageSource, Model};

const SID: &str = "sess_k4_inplace";
const TASK_TEXT: &str = "K4TASK 修复 login 页面的空指针";
const ASSISTANT_TEXT: &str = "定位到 main.rs 的 unwrap 修复中";
const QUEUED_TEXT: &str = "K4QUEUE 补跑一次完整回归";
const RELAY_SID: &str = "sess_k4_reflow_relay";
const RELAY_TASK: &str = "K4RELAY 给导出模块补 HTML 转义";

/// 进程级 env（ION_SESSION_DIR）+ session file override 是共享状态，串行化本文件所有测试。
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn faux_model() -> Model {
    Model {
        id: "faux-test".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}

fn config() -> AgentConfig {
    AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    }
}

fn user_msg(text: &str, ts: i64, source: MessageSource) -> Message {
    Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        timestamp: ts,
        source,
    })
}

fn user_text_of(m: &Message) -> String {
    match m {
        Message::User(u) => u
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn last_assistant_text(agent: &Agent) -> String {
    agent
        .messages()
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn context_user_texts(context: &ion_provider::types::Context) -> Vec<String> {
    context
        .messages
        .iter()
        .map(|m| match m {
            Message::User(u) => u
                .content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Message::Assistant(a) => a
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// K4 引导轮工厂（一次性消费）：断言续作引导轮看到了完整历史（任务 + 中断前回复）。
fn inplace_history_step(history_seen: Arc<AtomicUsize>) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Factory(Box::new(move |context, _opts, _state, _model| {
        let texts = context_user_texts(context);
        let has_task = texts.iter().any(|t| t.contains(TASK_TEXT));
        let has_prev_reply = texts.iter().any(|t| t.contains(ASSISTANT_TEXT));
        if has_task && has_prev_reply {
            history_seen.fetch_add(1, Ordering::SeqCst);
            faux::faux_assistant_message(
                faux::FauxContent::Text("RESUMED_FULL_HISTORY: 完整历史都在，继续任务".into()),
                faux::FauxMessageOptions::default(),
            )
        } else if has_task {
            faux::faux_assistant_message(
                faux::FauxContent::Text("PARTIAL_TASK_ONLY: 缺中断前回复".into()),
                faux::FauxMessageOptions::default(),
            )
        } else {
            faux::faux_assistant_message(
                faux::FauxContent::Text("MISSED: 没有看到任务历史".into()),
                faux::FauxMessageOptions::default(),
            )
        }
    }))
}

/// K4 排队消费轮工厂（一次性消费）：回放的排队 followUp 在引导轮之后自动开新轮投递
///（followUp 语义=下一轮投递，与运行时排队行为一致），该轮必须见到排队文本。
fn inplace_queue_step(queue_seen: Arc<AtomicUsize>) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Factory(Box::new(move |context, _opts, _state, _model| {
        let texts = context_user_texts(context);
        if texts.iter().any(|t| t.contains(QUEUED_TEXT)) {
            queue_seen.fetch_add(1, Ordering::SeqCst);
            faux::faux_assistant_message(
                faux::FauxContent::Text("QUEUE_DELIVERED: 排队消息已投递".into()),
                faux::FauxMessageOptions::default(),
            )
        } else {
            faux::faux_assistant_message(
                faux::FauxContent::Text("MISSED_QUEUE: 排队消息没有进上下文".into()),
                faux::FauxMessageOptions::default(),
            )
        }
    }))
}

/// 接力（降级）工厂：任务文本经摘要提示词进入上下文才答 RELAYED_OK。
fn relay_factory_step(seen: Arc<AtomicUsize>) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Factory(Box::new(move |context, _opts, _state, _model| {
        let texts = context_user_texts(context);
        if texts.iter().any(|t| t.contains(RELAY_TASK)) {
            seen.fetch_add(1, Ordering::SeqCst);
            faux::faux_assistant_message(
                faux::FauxContent::Text("RELAYED_OK: 从摘要提示词拿到原始任务".into()),
                faux::FauxMessageOptions::default(),
            )
        } else {
            faux::faux_assistant_message(
                faux::FauxContent::Text("MISSED_RELAY: 摘要提示词没有携带任务".into()),
                faux::FauxMessageOptions::default(),
            )
        }
    }))
}

fn static_step(text: &str) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text(text.into()),
        faux::FauxMessageOptions::default(),
    ))
}

fn new_registry_with(steps: Vec<faux::FauxResponseStep>) -> Arc<ApiRegistry> {
    let mut registry = ApiRegistry::new();
    let handle = faux::register_faux(&mut registry);
    handle.set_responses(steps);
    Arc::new(registry)
}

fn new_agent(registry: Arc<ApiRegistry>) -> Agent {
    Agent::new(
        registry,
        faux_model(),
        None,
        ion::agent::tool::ToolRegistry::new(),
        config(),
    )
}

/// 模拟 save_worker_session：把 agent 消息逐条追加为 type=message 条目（parentId 链）。
fn save_messages_like_worker(cwd: &str, msgs: &[Message]) {
    let path = ion::session_jsonl::resolve_session_file(cwd);
    let existing_msgs = std::fs::read_to_string(&path)
        .map(|c| {
            c.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
                .count()
        })
        .unwrap_or(0);
    let mut parent = SID.to_string();
    for m in msgs.iter().skip(existing_msgs) {
        let id = ion::session_jsonl::generate_id();
        let entry = serde_json::json!({
            "id": id,
            "parentId": parent,
            "timestamp": ion::session_jsonl::timestamp_iso(),
            "type": "message",
            "message": serde_json::to_value(m).unwrap(),
        });
        ion::session_jsonl::append_raw_entry(cwd, &entry);
        parent = id;
    }
}

/// K4 原地重建 worker 语义（fork-child 布局，ION_FORK_CHILD=1）：
/// 按 <sid>.jsonl 预加载全历史 → with_messages → queued_input 回放 → queue_replayed。
fn rebuild_agent_inplace(cwd: &str, steps: Vec<faux::FauxResponseStep>) -> Agent {
    let preloaded = ion::session_jsonl::SessionFile::load(cwd)
        .map(|f| f.messages)
        .unwrap_or_default();
    let mut agent = new_agent(new_registry_with(steps)).with_messages(preloaded);
    let pending = ion::session_jsonl::replay_queued_inputs(cwd);
    for (_entry_id, data) in pending {
        let kind = data
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("followUp");
        if let Some(mv) = data.get("message")
            && let Ok(msg) = serde_json::from_value::<Message>(mv.clone())
        {
            agent.queue_replayed(kind, msg);
        }
    }
    agent
}

/// K4 重派决策用的死 worker record（远程：AUTO-RECOVERY 既有主场景）。
fn k4_dead_record(sid: &str, cwd: &str) -> WorkerRecord {
    WorkerRecord {
        worker_id: "wkr_k4dead".into(),
        session_id: sid.into(),
        project: "k4".into(),
        project_path: cwd.into(),
        model: "faux-test".into(),
        agent: "build".into(),
        status: WorkerStatus::Dead,
        channels: vec![],
        parent: None,
        children: vec![],
        host: Some("box1".into()),
        notify_parent: false,
        started_at: 0,
        last_heartbeat: 0,
        status_since: 0,
        died_at: None,
        stdin: None,
        pending: Default::default(),
        event_subscribers: vec![],
        parent_event_tx: None,
        ready_tx: None,
        stdout_rx: None,
        response_rx: None,
        child_process: None,
        worktree: None,
        latest_output: VecDeque::new(),
        log_short: None,
        model_size: None,
        exit_code: Some(1),
        exit_reason: None,
        stderr_path: None,
        event_history: VecDeque::new(),
        event_history_cap: 1,
    }
}

/// worker post-run 消费标记语义：save 后按消息对账追加 consumed 标记。
fn mark_consumed_like_worker(cwd: &str, agent: &Agent) {
    let msgs_json: Vec<serde_json::Value> = agent
        .messages()
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    ion::session_jsonl::mark_queued_inputs_consumed(cwd, &msgs_json, "consumed");
}

/// 会话文件里含指定文本的 message 条目数——跨重建不变量：恰好一份。
fn count_message_entries_containing(cwd: &str, needle: &str) -> usize {
    let path = ion::session_jsonl::resolve_session_file(cwd);
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
        .filter_map(|v| v.get("message").cloned())
        .filter_map(|mv| serde_json::from_value::<Message>(mv).ok())
        .map(|m| user_text_of(&m))
        .filter(|t| t.contains(needle))
        .count()
}

fn file_message_count(cwd: &str) -> usize {
    let path = ion::session_jsonl::resolve_session_file(cwd);
    std::fs::read_to_string(path)
        .map(|c| {
            c.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
                .count()
        })
        .unwrap_or(0)
}

/// 测试隔离：ION_SESSION_DIR 指向临时目录 + session file override 指向 <sid>.jsonl
///（fork-child 布局，与远程 worker / 原地重建 respawn 的预加载路径一致）。
fn setup_session(tag: &str, sid: &str) -> (MutexGuard<'static, ()>, String, std::path::PathBuf) {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "ion_k4_inplace_{}_{}_{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().to_string();
    // 隔离铁律：绝不读写真实 ~/.ion——ION_SESSION_DIR 指向本次测试的临时目录
    unsafe { std::env::set_var("ION_SESSION_DIR", &dir) };
    let file = ion::paths::session_jsonl_path_by_id(&cwd, sid);
    ion::session_jsonl::set_session_file_override(Some(file));
    ion::session_jsonl::ensure_session_header(&cwd, sid);
    (guard, cwd, dir)
}

fn cleanup(guard: MutexGuard<'static, ()>, dir: &std::path::Path) {
    ion::session_jsonl::set_session_file_override(None);
    unsafe { std::env::remove_var("ION_SESSION_DIR") };
    let _ = std::fs::remove_dir_all(dir);
    drop(guard);
}

use std::sync::MutexGuard;

// ─────────────────────────────────────────────────────────────────────────────
// T1 原地重建全链路：跑到一半死 → 原地重派（原 sid）→ 全历史预加载 + 队列回放
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t1_inplace_rebuild_preloads_full_history_and_replays_queue() {
    let (guard, cwd, dir) = setup_session("t1_inplace", SID);
    let pre_crash_count;
    {
        // ── 前棒：agent 跑到一半（任务 + 中断前回复已落盘）──
        let mut agent_a = new_agent(new_registry_with(vec![static_step(ASSISTANT_TEXT)]));
        agent_a.run(TASK_TEXT).await.expect("A run");
        save_messages_like_worker(&cwd, agent_a.messages());
        pre_crash_count = file_message_count(&cwd);
        assert!(pre_crash_count >= 2, "task + reply must be persisted");

        // ── 忙时排队（H3：入队即落盘，未消费）──
        let queued = user_msg(QUEUED_TEXT, 333_333, MessageSource::FollowUp);
        let entry_id = ion::session_jsonl::append_queued_input(&cwd, "followUp", &queued);
        assert!(entry_id.is_some(), "queued_input entry must persist");
        // crash：直接 drop（内存队列蒸发，只有磁盘上的排队条目幸存）
    }

    // ── 重派决策（真实 try_auto_respawn，host=Some 远程主场景）──
    let record = k4_dead_record(SID, &cwd);
    let plan = ion::worker_registry::try_auto_respawn(&record, Some(1))
        .expect("session file at primary must decide a respawn plan");
    assert_eq!(
        plan.session.as_deref(),
        Some(SID),
        "in-place rebuild must keep the original sid"
    );
    assert_eq!(plan.strategy, "inplace_primary");
    assert_eq!(plan.project_path.as_deref(), Some(cwd.as_str()));
    assert!(plan.prompt.contains("原地重建"), "short guidance marker");
    assert!(
        !plan.prompt.contains(TASK_TEXT),
        "guidance must not re-quote the task (history is preloaded)"
    );

    // ── 原地重建：fork-child 语义（同 sid 全历史预加载 + 队列回放）──
    let history_seen = Arc::new(AtomicUsize::new(0));
    let queue_seen = Arc::new(AtomicUsize::new(0));
    let mut agent_b = rebuild_agent_inplace(
        &cwd,
        vec![
            inplace_history_step(history_seen.clone()),
            inplace_queue_step(queue_seen.clone()),
        ],
    );
    let preloaded = agent_b.messages().len();
    assert_eq!(
        preloaded, pre_crash_count,
        "full history must be preloaded from the original session file"
    );

    // ── 续作引导开新轮（真实链路：initial_prompt 经 prompt RPC 进为 user 轮）──
    // 注：回放的排队 followUp 在引导轮之后自动开新轮消费（followUp 语义=下一轮投递，
    // 与运行时行为一致），所以本次 run 两个 LLM turn：引导轮 + 排队消费轮。
    agent_b.run(&plan.prompt).await.expect("B run");
    let all_assistant: String = agent_b
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_assistant.contains("RESUMED_FULL_HISTORY"),
        "guidance turn must see full preloaded history, got: {all_assistant}"
    );
    assert!(
        all_assistant.contains("QUEUE_DELIVERED"),
        "replayed queue turn must deliver the queued message, got: {all_assistant}"
    );
    assert_eq!(
        history_seen.load(Ordering::SeqCst),
        1,
        "factory verified full history exactly once"
    );
    assert_eq!(
        queue_seen.load(Ordering::SeqCst),
        1,
        "factory verified queue delivery exactly once"
    );
    let all_user: String = agent_b
        .messages()
        .iter()
        .map(|m| user_text_of(m))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_user.contains(QUEUED_TEXT),
        "replayed queued message must be consumed as a real user turn"
    );

    // 消息数对得上：预加载历史 + 引导轮 + 排队消费轮（≥ 预加载 + 2 个 user 轮 + 2 个回复）
    assert!(
        agent_b.messages().len() >= preloaded + 4,
        "messages = preloaded history + guidance turn + replayed queue turn + replies, got {}",
        agent_b.messages().len()
    );

    // ── 收尾：save + 消费标记 → 排队消息在文件里恰好一份 ──
    save_messages_like_worker(&cwd, agent_b.messages());
    mark_consumed_like_worker(&cwd, &agent_b);
    assert_eq!(
        count_message_entries_containing(&cwd, QUEUED_TEXT),
        1,
        "queued message must appear exactly once in the session file"
    );
    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T2 降级链路：无文件 / header-only → 不重派（现状行为不变）
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t2_degrade_no_file_and_header_only_mean_no_respawn() {
    let (guard, cwd, dir) = setup_session("t2_degrade", "sess_k4_degrade");

    // 无文件：重派决策 None（现状：resolve 失败即不重派）
    let ghost = k4_dead_record("sess_k4_ghost", &cwd);
    assert!(
        ion::worker_registry::try_auto_respawn(&ghost, Some(1)).is_none(),
        "no session file anywhere → no respawn (current behavior)"
    );

    // header-only 文件：没有可预加载的历史，摘要接力也生成失败 → 不重派
    let sid = "sess_k4_degrade";
    let record = k4_dead_record(sid, &cwd);
    assert!(
        ion::worker_registry::try_auto_respawn(&record, Some(1)).is_none(),
        "header-only session file → no respawn (current behavior)"
    );

    // 对照：同样位置写入真实内容后 → 原地重建决策出现
    let file = ion::paths::session_jsonl_path_by_id(&cwd, sid);
    let lines = [
        r#"{"type":"session","id":"k4","agent":"build"}"#,
        r#"{"type":"message","message":{"User":{"content":"K4DEGRADE 真实任务","role":"user"}}}"#,
        r#"{"type":"message","message":{"Assistant":{"content":[{"Text":{"text":"进行中"}}],"role":"assistant"}}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();
    let plan = ion::worker_registry::try_auto_respawn(&record, Some(1))
        .expect("real content at primary must respawn");
    assert_eq!(plan.session.as_deref(), Some(sid), "must keep original sid");
    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T3 回流扫描 + 本机 respawn 降级接力：新 sid 空白重建，任务经摘要提示词达意
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t3_reflow_scan_local_respawn_downgrades_to_summary_relay_chain() {
    let (guard, cwd, dir) = setup_session("t3_relay", "sess_k3_unused");

    // 回流副本落在非预期目录（扫描命中），project_path（cwd 的 hash 目录）下没有它
    let sid = RELAY_SID;
    let other_dir = dir.join("--feedbeef--reflowed--");
    std::fs::create_dir_all(&other_dir).unwrap();
    let relay_file = other_dir.join(format!("{sid}.jsonl"));
    let relay_lines = [
        r#"{"type":"session","id":"k4","agent":"build"}"#,
        r#"{"type":"message","message":{"User":{"content":"K4RELAY 给导出模块补 HTML 转义","role":"user"}}}"#,
        r#"{"type":"message","message":{"Assistant":{"content":[{"Text":{"text":"已完成一半"}}],"role":"assistant"}}}"#,
    ];
    std::fs::write(&relay_file, relay_lines.join("\n")).unwrap();

    // 本机 respawn（host=None + allow_local 语义由调用方把关 → plan_auto_respawn local=true）
    let mut record = k4_dead_record(sid, &cwd);
    record.host = None;
    let plan = ion::worker_registry::plan_auto_respawn(
        &ion::paths::sessions_dir(),
        &record,
        true,
    )
    .expect("scan hit must still offer the summary-relay fallback");
    assert!(
        plan.session.is_none(),
        "local respawn on scan hit must downgrade to a new sid"
    );
    assert_eq!(plan.strategy, "summary_relay");
    assert!(
        plan.prompt.contains(RELAY_TASK),
        "relay prompt must carry the original task"
    );
    assert!(plan.prompt.contains("接力纪律"), "discipline preserved");

    // 降级接力重建：新 sid 空白 worker，任务只能靠摘要提示词进入上下文
    let seen = Arc::new(AtomicUsize::new(0));
    let mut agent_r = new_agent(new_registry_with(vec![relay_factory_step(seen.clone())]));
    agent_r.run(&plan.prompt).await.expect("relay run");
    let reply = last_assistant_text(&agent_r);
    assert!(
        reply.contains("RELAYED_OK"),
        "relay worker must receive the task via summary prompt, got: {reply}"
    );
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    cleanup(guard, &dir);
}
