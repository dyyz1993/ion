//! queued_input 排队消息落盘持久化 harness（H3：steer/followUp 队列 crash 不丢）。
//!
//! 背景：worker 的 steering/followUp/next_turn 队列是纯内存 VecDeque——kill -9 / crash
//! 时排队消息全部丢失（用户消息凭空蒸发）。修复 = 「入队即落盘 / 消费即标记」：
//!   排队：custom/queued_input         data={kind,text,images?,queuedAt,message}
//!   消费：custom/queued_input_consumed data={entryId,reason,consumedAt}
//! 回放：worker 重建时取「无消费标记 + 未被会话消息吸收 + 仍在 live path」的条目重新入队。
//!
//! 验收标准（对齐历史教训「排队回执 ≠ 送达」）：断言的是新轮**真实作答**——
//! FauxProvider Factory 检查 LLM 上下文里是否真的出现了排队文本再分支返回，
//! 不以"队列里有消息"为准；跨重建的不变量是"会话文件里排队消息恰好一份"。
//!
//! 场景：
//!   T1 黑洞复现（修复前红）：纯内存 enqueue → drop → 重建 → 消息蒸发（工厂答 MISSED）。
//!   T2 修复后绿：入队即落盘 → drop → 重建 + 回放 → 新轮真实作答（工厂答 ACK）。
//!   T3 幂等：重建两次 → 只投递一次；消费标记后第三次重建不再重复入历史。
//!   T4 steer 路由 + 消费正常路径回归（忙时排队 → 下轮消费 → 落盘无残留）。
//!   T5 数据层单测：append → 未消费列表有序 → consumed/removed 标记 → 列表收敛。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::messages::{ContentBlock, Message, TextContent, UserMessage};
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::{AssistantContentBlock, MessageSource, Model};

const QUEUED_TEXT: &str = "QUEUE_ME_H3_PERSIST 检查排队消息是否真的进上下文";
const SID: &str = "sess_h3_queue_persist";

/// 进程级全局 override + cwd 解析是共享状态，串行化本文件所有测试。
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

/// 上下文里排队文本的出现次数（Factory 判据：消息真的进了 LLM 上下文）。
fn count_in_context(context: &ion_provider::types::Context) -> usize {
    context
        .messages
        .iter()
        .map(user_text_of)
        .filter(|t| t.contains(QUEUED_TEXT))
        .count()
}

/// Factory 步骤：检查上下文 → seen 计数 → 按是否见到排队文本分支作答。
fn factory_step(seen: Arc<AtomicUsize>) -> faux::FauxResponseStep {
    faux::FauxResponseStep::Factory(Box::new(move |context, _opts, _state, _model| {
        if count_in_context(context) > 0 {
            seen.fetch_add(1, Ordering::SeqCst);
            faux::faux_assistant_message(
                faux::FauxContent::Text("ACK_QUEUE: 我看到了排队消息".into()),
                faux::FauxMessageOptions::default(),
            )
        } else {
            faux::faux_assistant_message(
                faux::FauxContent::Text("MISSED_QUEUE: 没有看到排队消息".into()),
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

fn new_registry_with(steps: Vec<faux::FauxResponseStep>) -> (Arc<ApiRegistry>, Arc<faux::FauxProvider>) {
    let mut registry = ApiRegistry::new();
    let handle = faux::register_faux(&mut registry);
    handle.set_responses(steps);
    (Arc::new(registry), handle)
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
/// 对齐真实实现的去重语义：按文件已有 message 条目数跳过头部已落盘的消息。
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

/// worker 重建语义：SessionFile::load → with_messages → 回放未消费 queued_input → queue_replayed。
fn rebuild_agent(cwd: &str, steps: Vec<faux::FauxResponseStep>) -> Agent {
    let preloaded = ion::session_jsonl::SessionFile::load(cwd)
        .map(|f| f.messages)
        .unwrap_or_default();
    let (registry, _handle) = new_registry_with(steps);
    let mut agent = new_agent(registry).with_messages(preloaded);
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

/// worker post-run 消费标记语义：save_worker_session 后按消息对账追加 consumed 标记。
fn mark_consumed_like_worker(cwd: &str, agent: &Agent) {
    let msgs_json: Vec<serde_json::Value> = agent
        .messages()
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    ion::session_jsonl::mark_queued_inputs_consumed(cwd, &msgs_json, "consumed");
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

/// 会话文件里排队消息（message 条目）出现的次数——跨重建的不变量：恰好一份。
fn count_queued_in_file(cwd: &str) -> usize {
    let path = ion::session_jsonl::resolve_session_file(cwd);
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("message"))
        .filter_map(|v| v.get("message").cloned())
        .filter_map(|mv| serde_json::from_value::<Message>(mv).ok())
        .map(|m| user_text_of(&m))
        .filter(|t| t.contains(QUEUED_TEXT))
        .count()
}

fn setup_session(tag: &str) -> (std::sync::MutexGuard<'static, ()>, String, std::path::PathBuf) {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "ion_h3_queue_{}_{}_{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().to_string();
    // 隔离铁律：绝不读写真实 ~/.ion——ION_SESSION_DIR 指向本次测试的临时目录
    // （TEST_LOCK 串行化本进程内所有测试，env 是进程级全局状态）
    unsafe { std::env::set_var("ION_SESSION_DIR", &dir) };
    let file = ion::paths::session_jsonl_path_by_id(&cwd, SID);
    ion::session_jsonl::set_session_file_override(Some(file.clone()));
    ion::session_jsonl::ensure_session_header(&cwd, SID);
    (guard, cwd, dir)
}

fn cleanup(guard: std::sync::MutexGuard<'static, ()>, dir: &std::path::Path) {
    ion::session_jsonl::set_session_file_override(None);
    unsafe { std::env::remove_var("ION_SESSION_DIR") };
    let _ = std::fs::remove_dir_all(dir);
    drop(guard);
}

// ─────────────────────────────────────────────────────────────────────────────
// T1 黑洞复现（修复前的红）：纯内存 enqueue，kill 后重建，消息蒸发。
// 只用旧路径语义（内存队列，无落盘、无回放）——文档化 bug 本体。
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t1_blackhole_in_memory_queue_lost_on_death() {
    let (guard, cwd, dir) = setup_session("t1_blackhole");
    {
        let (registry, _h) = new_registry_with(vec![static_step("first answer")]);
        let mut agent_a = new_agent(registry);
        agent_a.run("initial prompt").await.expect("A run");

        // 模拟 worker 旧行为：忙时 enqueue 只进内存（不落盘）
        agent_a.follow_up(user_msg(QUEUED_TEXT, 111_111, MessageSource::FollowUp));
        // kill -9：直接 drop，队列蒸发
    }

    // 同 sid 重建（只有磁盘上的消息；无回放——修复前行为）
    let seen = Arc::new(AtomicUsize::new(0));
    let (registry, _h) = new_registry_with(vec![factory_step(seen.clone())]);
    let preloaded = ion::session_jsonl::SessionFile::load(&cwd)
        .map(|f| f.messages)
        .unwrap_or_default();
    let mut agent_b = new_agent(registry).with_messages(preloaded);
    agent_b.run("new prompt").await.expect("B run");

    // 断言：排队消息蒸发（修复前红）——新轮 LLM 没见到它
    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "黑洞复现：排队消息不该出现在重建后的上下文"
    );
    assert!(
        last_assistant_text(&agent_b).contains("MISSED_QUEUE"),
        "黑洞复现：工厂应答 MISSED，实际: {}",
        last_assistant_text(&agent_b)
    );
    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T2 修复后绿：入队即落盘 → kill → 重建+回放 → 新轮真实作答（ACK）。
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t2_persist_replay_delivers_in_new_run() {
    let (guard, cwd, dir) = setup_session("t2_green");
    let ts: i64 = 222_222;
    {
        let (registry, _h) = new_registry_with(vec![static_step("first answer")]);
        let mut agent_a = new_agent(registry);
        agent_a.run("initial prompt").await.expect("A run");
        save_messages_like_worker(&cwd, agent_a.messages());

        // 修复后入队路径：入队即落盘（queued_input 条目）+ 内存入队
        let msg = user_msg(QUEUED_TEXT, ts, MessageSource::FollowUp);
        let entry_id = ion::session_jsonl::append_queued_input(&cwd, "followUp", &msg);
        assert!(entry_id.is_some(), "queued_input 条目应落盘成功");
        agent_a.follow_up(msg);
        drop(agent_a); // kill -9
    }

    // 重建：load + 回放（queued 文本还在盘上，未被消费）。
    // 两步：turn1（排队消息还在队列，未进上下文）→ outer_loop drain → turn2 见到并作答。
    let seen = Arc::new(AtomicUsize::new(0));
    let mut agent_b = rebuild_agent(
        &cwd,
        vec![static_step("turn one"), factory_step(seen.clone())],
    );
    agent_b.run("new prompt").await.expect("B run");

    // 验收铁律：断言新轮真实作答（工厂在上下文里见到了排队文本才答 ACK）
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "回放消息应恰好在一次 LLM 调用的上下文里出现"
    );
    assert!(
        last_assistant_text(&agent_b).contains("ACK_QUEUE"),
        "修复后应真实作答 ACK，实际: {}",
        last_assistant_text(&agent_b)
    );

    // post-run：save + 消费标记 → 会话文件里恰好一份，盘上无待回放残留
    save_messages_like_worker(&cwd, agent_b.messages());
    mark_consumed_like_worker(&cwd, &agent_b);
    assert_eq!(
        count_queued_in_file(&cwd),
        1,
        "会话文件里排队消息应恰好一份"
    );
    assert!(
        ion::session_jsonl::replay_queued_inputs(&cwd).is_empty(),
        "消费后回放列表应为空"
    );
    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T3 幂等：重建两次 → 只投递一次；消费标记后第三次重建历史里不出现第二份。
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t3_double_rebuild_delivers_exactly_once() {
    let (guard, cwd, dir) = setup_session("t3_idempotent");
    let ts: i64 = 333_333;
    {
        let (registry, _h) = new_registry_with(vec![static_step("first")]);
        let mut agent_a = new_agent(registry);
        agent_a.run("initial").await.expect("A run");
        save_messages_like_worker(&cwd, agent_a.messages());
        let msg = user_msg(QUEUED_TEXT, ts, MessageSource::FollowUp);
        ion::session_jsonl::append_queued_input(&cwd, "followUp", &msg);
        agent_a.follow_up(msg);
        drop(agent_a);
    }

    // 重建 1：回放入队（未消费，未跑）→ 再次死亡
    let agent_b1 = rebuild_agent(&cwd, vec![]);
    assert_eq!(agent_b1.follow_up_queue_len(), 1, "重建 1 应回放 1 条");
    drop(agent_b1);

    // 重建 2：再回放——仍只有一条待投递（每次重建都是全新 agent，不叠加）
    let seen = Arc::new(AtomicUsize::new(0));
    let mut agent_b2 = rebuild_agent(
        &cwd,
        vec![static_step("turn one"), factory_step(seen.clone())],
    );
    assert_eq!(agent_b2.follow_up_queue_len(), 1, "重建 2 回放不叠加");
    agent_b2.run("new prompt").await.expect("B2 run");
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "重建两次后新轮投递必须恰好一次（不双投递）"
    );
    assert!(last_assistant_text(&agent_b2).contains("ACK_QUEUE"));

    // post-run：save + 消费标记
    save_messages_like_worker(&cwd, agent_b2.messages());
    mark_consumed_like_worker(&cwd, &agent_b2);

    // 重建 3：零回放；带完整历史跑一轮后，文件里排队消息仍只有一份
    let mut agent_b3 = rebuild_agent(&cwd, vec![static_step("tail")]);
    assert!(
        ion::session_jsonl::replay_queued_inputs(&cwd).is_empty(),
        "消费标记后不应再有该排队条目"
    );
    agent_b3.run("another prompt").await.expect("B3 run");
    save_messages_like_worker(&cwd, agent_b3.messages());
    assert_eq!(
        count_queued_in_file(&cwd),
        1,
        "消费标记后再次重建+运行，历史里排队消息绝不能出现第二份"
    );
    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T4 steer 路由 + 消费正常路径回归：
//   a) steer 回放 → steering_queue → 下轮 turn 开始即注入（drain_steering 原语义）
//   b) 忙时排队（入队即落盘）→ run 内 drain → 消息进对话 → post-run 标记无残留
// ─────────────────────────────────────────────────────────────────────────────
#[tokio::test]
async fn t4_steer_routing_and_normal_consumption_regression() {
    let (guard, cwd, dir) = setup_session("t4_steer");
    let ts: i64 = 444_444;

    // ── a) steer 回放路由 ──
    {
        let (registry, _h) = new_registry_with(vec![static_step("first")]);
        let mut agent_a = new_agent(registry);
        agent_a.run("initial").await.expect("A run");
        save_messages_like_worker(&cwd, agent_a.messages());
        let msg = user_msg(QUEUED_TEXT, ts, MessageSource::Steer);
        ion::session_jsonl::append_queued_input(&cwd, "steer", &msg);
        agent_a.steer(msg);
        drop(agent_a);
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let mut agent_b = rebuild_agent(&cwd, vec![factory_step(seen.clone())]);
    // 回放按 kind 路由：steer → steering_queue（原有消费语义不动）
    assert_eq!(
        agent_b.steering_queue_len(),
        1,
        "steer 回放应进 steering_queue"
    );
    // drain_steering 在 inner_loop 开始消费 → 第一次 LLM 调用就见到
    agent_b.run("prompt after rebuild").await.expect("B run");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "steer 回放应投递一次");
    assert!(last_assistant_text(&agent_b).contains("ACK_QUEUE"));
    mark_consumed_like_worker(&cwd, &agent_b);
    assert!(ion::session_jsonl::replay_queued_inputs(&cwd).is_empty());

    // ── b) 消费正常路径回归：忙时排队 → 同一 run 内 drain 消费 → 无残留 ──
    let ts2: i64 = 555_555;
    let seen2 = Arc::new(AtomicUsize::new(0));
    {
        let (registry, handle) = new_registry_with(vec![
            static_step("turn one"),
            factory_step(seen2.clone()),
        ]);
        let mut agent_c = new_agent(registry);
        let msg = user_msg(QUEUED_TEXT, ts2, MessageSource::FollowUp);
        ion::session_jsonl::append_queued_input(&cwd, "followUp", &msg);
        agent_c.follow_up(msg);
        agent_c.run("initial c").await.expect("C run");
        // 现有行为回归：outer_loop 在 inner_loop 之间 drain follow_up_queue → 新 turn
        assert_eq!(seen2.load(Ordering::SeqCst), 1, "排队消息应进下一 turn");
        assert!(
            last_assistant_text(&agent_c).contains("ACK_QUEUE"),
            "正常排队路径应真实作答 ACK，实际: {}",
            last_assistant_text(&agent_c)
        );
        let _ = handle;
        // post-run 标记 → 无残留
        mark_consumed_like_worker(&cwd, &agent_c);
        assert!(
            ion::session_jsonl::replay_queued_inputs(&cwd).is_empty(),
            "正常消费后回放列表应为空（回归：不双投递）"
        );
    }

    cleanup(guard, &dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// T5 数据层单测：append → 未消费列表有序 → consumed/removed 标记 → 列表收敛。
// 不跑 agent，纯 JSONL 层。
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t5_session_jsonl_queued_entry_lifecycle() {
    let (guard, cwd, dir) = setup_session("t5_lifecycle");
    let m1 = user_msg("first queued", 1000, MessageSource::FollowUp);
    let m2 = user_msg("second queued", 2000, MessageSource::Steer);
    let id1 = ion::session_jsonl::append_queued_input(&cwd, "followUp", &m1)
        .expect("append m1");
    let id2 = ion::session_jsonl::append_queued_input(&cwd, "steer", &m2).expect("append m2");
    assert_ne!(id1, id2);

    // 有序：先排队的先回放；kind/message 字段保真
    let pending = ion::session_jsonl::replay_queued_inputs(&cwd);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].0, id1);
    assert_eq!(
        pending[0].1.get("kind").and_then(|v| v.as_str()),
        Some("followUp")
    );
    assert_eq!(
        pending[1].1.get("kind").and_then(|v| v.as_str()),
        Some("steer")
    );
    assert!(pending[1].1.get("message").is_some(), "完整 message 应在 data 里");

    // 消费 m1（按序列化消息对账）
    let m1_json = serde_json::to_value(&m1).unwrap();
    let marked = ion::session_jsonl::mark_queued_inputs_consumed(&cwd, &[m1_json], "consumed");
    assert_eq!(marked, 1, "应恰好标记 1 条");
    let pending = ion::session_jsonl::replay_queued_inputs(&cwd);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, id2);

    // removed 标记 m2（对应 remove_follow_up / clear_queue 语义）
    let m2_json = serde_json::to_value(&m2).unwrap();
    let marked = ion::session_jsonl::mark_queued_inputs_consumed(&cwd, &[m2_json], "removed");
    assert_eq!(marked, 1);
    assert!(ion::session_jsonl::replay_queued_inputs(&cwd).is_empty());

    // 重复对账幂等：已消费的消息再对账 → 0 条新标记
    let m1_json = serde_json::to_value(&m1).unwrap();
    let marked = ion::session_jsonl::mark_queued_inputs_consumed(&cwd, &[m1_json], "consumed");
    assert_eq!(marked, 0, "重复对账不应产生新标记");

    cleanup(guard, &dir);
}
