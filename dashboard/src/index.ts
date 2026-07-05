/**
 * ION Dashboard —— OpenTUI 入口
 *
 * 启动流程：
 * 1. createCliRenderer
 * 2. 注册 rerender 函数（重建整树）
 * 3. 注册键盘快捷键
 * 4. setInterval 每秒 poll get_overview
 */
import { createCliRenderer, Box, Text } from "@opentui/core";
import { pollOverview, subscribeSession, getMessages } from "./manager";
import { state, setRerenderFn, rerender, log, ChatMessage } from "./state";
import { renderRoot } from "./layout/root";
import { setupKeybinds } from "./keybinds";

async function main() {
  process.on("uncaughtException", (e: any) => {
    log(`uncaught: ${e.message}`);
  });
  process.on("unhandledRejection", (e: any) => {
    log(`unhandled: ${e}`);
  });

  const renderer = await createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 30,
  });

  setRerenderFn(() => {
    try {
      const root = renderer.root;
      // 关键：用 remove() 不用 destroyRecursively()，避免误伤 Input 单例
      const children = root.getChildren().slice();
      for (const child of children) {
        if (!child) continue;
        if (typeof root.remove === "function") {
          root.remove(child);
        } else if (typeof child.destroyRecursively === "function") {
          child.destroyRecursively();
        }
      }
      const tree = renderRoot(renderer);
      if (tree) root.add(tree);
    } catch (e: any) {
      log(`rerender error: ${e.message}`);
    }
  });

  setupKeybinds(renderer);
  rerender();
  log("renderer ready");

  // ── subscribe session 流式事件 ──
  // 当 selectedSessionId 变化时，重新订阅
  let lastSubscribed: string | null = null;
  let unsubscribe: (() => void) | null = null;

  const subscribeIfNeeded = () => {
    const sid = state.selectedSessionId;
    if (sid === lastSubscribed) return;
    lastSubscribed = sid;

    // 清理旧订阅
    if (unsubscribe) {
      try { unsubscribe(); } catch {}
      unsubscribe = null;
    }

    if (!sid) return;

    // 加载历史消息（一次性）
    getMessages(sid).then((msgs) => {
      const history: ChatMessage[] = [];
      for (const m of msgs) {
        const role = m.role || "assistant";
        // 提取文本 content
        let content = "";
        if (typeof m.content === "string") content = m.content;
        else if (Array.isArray(m.content)) {
          for (const c of m.content) {
            if (typeof c === "string") content += c;
            else if (c?.text) content += c.text;
          }
        }
        if (content) history.push({ role, content });
      }
      if (history.length > 0) {
        state.messages.set(sid, history);
        rerender();
      }
    }).catch(() => {});

    // 订阅实时事件
    unsubscribe = subscribeSession(sid, {
      onEvent: (event) => {
        const et = event.type || event.event?.type;
        if (et === "agent_start") {
          state.streamingSession = sid;
          // 准备接收流式 assistant 消息
          const msgs = state.messages.get(sid) || [];
          msgs.push({ role: "assistant", content: "", streaming: true });
          state.messages.set(sid, msgs);
          rerender();
        } else if (et === "text_delta") {
          const delta = event.delta || event.event?.delta || "";
          const msgs = state.messages.get(sid) || [];
          // 追加到最后一条 streaming assistant 消息
          const last = msgs[msgs.length - 1];
          if (last && last.streaming) {
            last.content += delta;
          } else {
            msgs.push({ role: "assistant", content: delta, streaming: true });
          }
          state.messages.set(sid, msgs);
          rerender();
        } else if (et === "agent_end") {
          const msgs = state.messages.get(sid) || [];
          const last = msgs[msgs.length - 1];
          if (last) last.streaming = false;
          state.streamingSession = null;
          rerender();
        } else if (et === "tool_execution_start") {
          const tool = event.toolName || event.event?.toolName || "?";
          log(`🔧 ${tool}`);
        }
      },
      onDisconnect: () => log(`subscribe ${sid?.slice(0,8)} 断开`),
    });
    log(`订阅 ${sid.slice(0, 8)}`);
  };

  // 每次 rerender 后检查是否需要重订阅
  const origRerender = rerender;
  // 注：rerender 是 state 模块的，我们在这里包装一下通过 setInterval 检查
  setInterval(() => {
    subscribeIfNeeded();
  }, 300);

  // 首次拉取
  try {
    const overview = await pollOverview();
    state.workers = overview.workers || [];
    state.projects = overview.projects || [];
    state.totalStale = overview.total_stale || 0;
    state.connected = true;
    log(`loaded ${state.workers.length} workers`);
    rerender();
  } catch (e: any) {
    state.connected = false;
    log(`connect failed: ${e.message}`);
    rerender();
  }

  // 轮询 Manager（每秒一次）
  setInterval(async () => {
    try {
      const overview = await pollOverview();
      state.workers = overview.workers || [];
      state.projects = overview.projects || [];
      state.totalStale = overview.total_stale || 0;
      state.connected = true;
      rerender();
    } catch (e) {
      if (state.connected) {
        state.connected = false;
        log("disconnected from manager");
        rerender();
      }
    }
  }, 1000);
}

main().catch((e) => {
  // 致命错误才写 stderr
  process.stderr.write(`[fatal] ${e.stack || e}\n`);
  process.exit(1);
});
