// webui/gateway.mjs — ion host socket ↔ HTTP/WebSocket bridge.
//
// Connects the browser to `ion serve`'s Unix socket (~/.ion/host.sock).
//   GET  /            → serves webui/index.html (and other static files)
//   POST /rpc         → forwards one JSON-RPC line to the host socket,
//                       reads back the line carrying the matching `id`, returns it.
//   WS   /ws?session= → opens a host socket, sends {method:"subscribe",session},
//                       forwards every subsequent line to the browser.
//
// ion's socket protocol is line-delimited JSON (JSONL). For RPC, the client sends
// {"id","method","params"[,"session"]}\n and the host replies with lines — only the
// line carrying the same `id` is the actual response; other lines are events and
// are skipped (see src/bin/ion.rs cmd_rpc). For subscribe, the host streams events
// line by line indefinitely.
//
// Usage:  node webui/gateway.mjs [--port 8787] [--ion-bin ./target/debug/ion]
//
// Requires the `ws` npm package for WebSocket server. (npm install)

import { createRequire } from "node:module";
import { createServer } from "node:http";
import { connect } from "node:net";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

const require = createRequire(import.meta.url);
const { WebSocketServer } = require("ws");

const __dirname = resolve(fileURLToPath(import.meta.url), "..");
const WEBUI_DIR = __dirname;
const HOST_SOCK = join(homedir(), ".ion", "host.sock");

// --- CLI args -------------------------------------------------------------
const argv = process.argv.slice(2);
function arg(name, def) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : def;
}
const PORT = parseInt(arg("port", "8787"), 10);
const ION_BIN = arg("ion-bin", "ion"); // used only for the /ensure hint

// --- helpers --------------------------------------------------------------
function log(...a) {
  console.error(`[gateway] ${new Date().toISOString()} ${a.join(" ")}`);
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".md": "text/markdown; charset=utf-8",
};

async function serveStatic(req, res) {
  // Only allow files inside webui/, default to index.html.
  let rel = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname));
  if (rel === "/" || rel === "") rel = "/index.html";
  rel = rel.replace(/^[/\\]+/, "");
  const abs = join(WEBUI_DIR, rel);
  // prevent path escape
  if (!abs.startsWith(WEBUI_DIR)) {
    res.writeHead(403);
    res.end("forbidden");
    return true;
  }
  try {
    const s = await stat(abs);
    if (s.isDirectory()) {
      res.writeHead(404);
      res.end("not a file");
      return true;
    }
    const body = await readFile(abs);
    res.writeHead(200, { "Content-Type": MIME[extname(abs)] || "application/octet-stream" });
    res.end(body);
    return true;
  } catch {
    return false;
  }
}

// Open a fresh line-delimited connection to the host socket.
function openHost() {
  return new Promise((resolveConn, rejectConn) => {
    const sock = connect(HOST_SOCK);
    sock.setEncoding("utf8");
    sock.setNoDelay(true);
    sock.once("connect", () => resolveConn(sock));
    sock.once("error", (e) => rejectConn(e));
  });
}

function readJsonBody(buf) {
  // Split buffer/string on newlines, parse each non-empty line.
  return buf
    .toString("utf8")
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      try {
        return JSON.parse(l);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

// --- HTTP POST /rpc -------------------------------------------------------
async function handleRpc(req, res) {
  // Body: the JSON to send as the RPC request. We inject `id` if absent.
  let raw = "";
  for await (const chunk of req) raw += chunk;
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    res.writeHead(400, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "request body is not valid JSON" }));
    return;
  }
  const id = payload.id != null ? payload.id : `gw-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  payload.id = id;

  let sock;
  try {
    sock = await openHost();
  } catch (e) {
    res.writeHead(502, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        error: `cannot connect to ion host socket at ${HOST_SOCK} — is 'ion serve' running? (${e.message})`,
      }),
    );
    return;
  }

  const reply = { data: [] }; // accumulate response + any events
  let buf = "";
  let settled = false;
  const timer = setTimeout(() => finish(504, { error: "host did not respond within 30s" }), 30000);

  function finish(status, body) {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    try {
      sock.destroy();
    } catch {}
    res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify(body));
  }

  sock.on("data", (chunk) => {
    buf += chunk.toString("utf8");
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      let obj;
      try {
        obj = JSON.parse(line);
      } catch {
        continue;
      }
      if (obj && obj.id === id) {
        finish(200, obj);
        return;
      }
      // else: an event that arrived before the response — collect it.
      reply.data.push(obj);
    }
  });
  sock.on("error", (e) => finish(502, { error: `socket error: ${e.message}` }));
  sock.on("close", () => finish(502, { error: "host closed connection without response" }));

  sock.write(JSON.stringify(payload) + "\n");
}

// --- HTTP server ----------------------------------------------------------
const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://x");

  if (req.method === "GET" && url.pathname === "/healthz") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ ok: true, sock: HOST_SOCK }));
    return;
  }

  if (req.method === "POST" && url.pathname === "/rpc") {
    return handleRpc(req, res);
  }

  if (req.method === "GET") {
    const ok = await serveStatic(req, res);
    if (ok) return;
  }

  res.writeHead(404, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ error: `not found: ${req.method} ${req.url}` }));
});

// --- WebSocket /ws --------------------------------------------------------
const wss = new WebSocketServer({ server, path: "/ws" });

wss.on("connection", (ws, req) => {
  const url = new URL(req.url, "http://x");
  const session = url.searchParams.get("session") || "";
  const extension = url.searchParams.get("extension") || "";
  const ui = url.searchParams.get("ui") === "1" || url.searchParams.get("ui") === "true";

  const sub = { method: "subscribe" };
  if (session) sub.session = session;
  if (extension) sub.extension = extension;
  if (ui) sub.ui = true;

  let sock;
  let buf = "";
  let closed = false;

  function send(obj) {
    if (closed) return;
    try {
      ws.send(JSON.stringify(obj));
    } catch {
      closed = true;
    }
  }

  (async () => {
    try {
      sock = await openHost();
    } catch (e) {
      send({ type: "gateway_error", error: `cannot connect to host socket: ${e.message}` });
      try {
        ws.close();
      } catch {}
      return;
    }
    sock.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line) continue;
        let obj;
        try {
          obj = JSON.parse(line);
        } catch {
          send({ type: "raw", line });
          continue;
        }
        send(obj);
      }
    });
    sock.on("error", (e) => send({ type: "gateway_error", error: `socket error: ${e.message}` }));
    sock.on("close", () => {
      send({ type: "gateway_closed" });
      try {
        ws.close();
      } catch {}
    });
    sock.write(JSON.stringify(sub) + "\n");
    send({ type: "gateway_subscribed", subscribe: sub });
  })();

  ws.on("message", (data) => {
    // Browser → host: forward raw (used for ui_respond or future commands).
    try {
      if (sock && !closed) sock.write(data.toString("utf8") + "\n");
    } catch {}
  });
  ws.on("close", () => {
    closed = true;
    try {
      sock && sock.destroy();
    } catch {}
  });
});

server.listen(PORT, () => {
  log(`listening on http://localhost:${PORT}`);
  log(`host socket: ${HOST_SOCK}`);
  log(`(if 'ion serve' is not running, requests to /rpc will return 502)`);
});

// Keep the process from exiting on stray errors; log instead.
process.on("uncaughtException", (e) => log("uncaughtException:", e.stack || e.message));
process.on("unhandledRejection", (e) => log("unhandledRejection:", e));
