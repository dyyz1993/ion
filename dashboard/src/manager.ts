/**
 * ION Manager Unix socket 客户端
 *
 * 两种模式：
 * - 一问一答 RPC（每次新建连接）：get_overview, create_session, prompt
 * - 长连接 subscribe（专用连接）：subscribe session 流式收事件
 */
import path from "node:path";
import os from "node:os";

const SOCK = path.join(os.homedir(), ".ion", "manager.sock");

/** 底层 RPC：发一个 JSON 请求，等一行 JSON 响应 */
function rpc(req: any): Promise<any> {
  return new Promise((resolve, reject) => {
    let buf = "";
    let settled = false;
    let socketRef: any = null;

    const settle = (fn: () => void) => {
      if (settled) return;
      settled = true;
      try { fn(); } catch (e) { reject(e); }
    };

    // 超时兜底（5 秒）
    const timeout = setTimeout(() => {
      settle(() => reject(new Error("rpc timeout (5s)")));
      if (socketRef) try { socketRef.end(); } catch {}
    }, 5000);

    try {
      Bun.connect({
        unix: SOCK,
        socket: {
          open(socket) {
            socketRef = socket;
            const line = JSON.stringify(req) + "\n";
            socket.write(line);
            socket.flush?.();
          },
          data(socket, chunk) {
            buf += chunk.toString();
            // 等到完整 JSON 行（含 } 后的换行）
            const nl = buf.indexOf("\n");
            if (nl !== -1) {
              clearTimeout(timeout);
              const line = buf.slice(0, nl).trim();
              try {
                const resp = JSON.parse(line);
                socket.end();
                settle(() => resolve(resp));
              } catch (e: any) {
                socket.end();
                settle(() => reject(new Error(`parse: ${e.message}`)));
              }
            }
          },
          error(_socket, err) {
            clearTimeout(timeout);
            settle(() => reject(new Error(`socket error: ${err.message || err}`)));
          },
          close() {
            // 如果 buf 里有内容但没换行，尝试解析
            clearTimeout(timeout);
            if (buf.trim() && !settled) {
              try {
                const resp = JSON.parse(buf.trim());
                settle(() => resolve(resp));
                return;
              } catch {}
            }
            settle(() => reject(new Error("socket closed before response")));
          },
        },
      }).catch((err: any) => {
        clearTimeout(timeout);
        settle(() => reject(new Error(`connect: ${err.message || err}`)));
      });
    } catch (e: any) {
      clearTimeout(timeout);
      reject(new Error(`connect throw: ${e.message}`));
    }
  });
}

/** 取 overview 快照 */
export async function pollOverview(): Promise<any> {
  const resp = await rpc({ method: "get_overview", id: "poll" });
  if (resp.success) return resp.data;
  throw new Error(resp.error || "get_overview failed");
}

/** 创建 session（自动 spawn worker） */
export async function createSession(projectPath: string, agent: string): Promise<any> {
  const resp = await rpc({
    method: "create_session",
    params: { project_path: projectPath, agent },
  });
  return resp;
}

/** 发送聊天消息 */
export async function sendPrompt(session: string, text: string): Promise<any> {
  const resp = await rpc({
    method: "prompt",
    session,
    params: { text },
  });
  return resp;
}

export const SOCKET_PATH = SOCK;

/**
 * 订阅 session 事件流（长连接）
 * 返回一个 unsubscribe 函数。
 *
 * 事件类型：
 * - instance_event/response: prompt 命令响应
 * - instance_event/agent_start: Agent 开始
 * - instance_event/event(text_delta): 流式文本
 * - instance_event/event(agent_end): Agent 结束
 * - instance_event/event(tool_*): 工具调用
 */
export function subscribeSession(
  session: string,
  handlers: {
    onEvent?: (event: any) => void;
    onDisconnect?: () => void;
  }
): () => void {
  let closed = false;
  let socketRef: any = null;
  let buf = "";

  const cleanup = () => {
    if (closed) return;
    closed = true;
    try { socketRef?.end?.(); } catch {}
    handlers.onDisconnect?.();
  };

  try {
    Bun.connect({
      unix: SOCK,
      socket: {
        open(socket) {
          socketRef = socket;
          socket.write(JSON.stringify({ method: "subscribe", session }) + "\n");
        },
        data(socket, chunk) {
          buf += chunk.toString();
          // 按行处理
          let nl;
          while ((nl = buf.indexOf("\n")) !== -1) {
            const line = buf.slice(0, nl).trim();
            buf = buf.slice(nl + 1);
            if (!line) continue;
            try {
              const msg = JSON.parse(line);
              // 提取嵌套的 event（instance_event 包了一层）
              let event = msg;
              if (msg.type === "instance_event" && msg.event) {
                event = msg.event;
              }
              handlers.onEvent?.(event);
            } catch {}
          }
        },
        error() { cleanup(); },
        close() { cleanup(); },
      },
    }).catch(() => cleanup());
  } catch {
    cleanup();
  }

  return cleanup;
}

/** 取历史消息（进 Focus 模式时加载） */
export async function getMessages(session: string): Promise<any[]> {
  const resp = await rpc({
    method: "get_messages",
    session,
    id: "getmsg",
  });
  // 响应格式: {data: {messages: [...]}} 或 {data: [...]}
  if (resp.success) {
    return resp.data?.messages || resp.data || [];
  }
  return [];
}
