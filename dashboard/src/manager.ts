/**
 * ION Manager Unix socket 客户端
 *
 * Manager 是一问一答模式：每个请求新建一个连接。
 * 这个模块封装了 RPC 调用，对上层透明。
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
