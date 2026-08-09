#!/usr/bin/env node
// ION Dashboard WebSocket 网关 v2
//
// 关键架构修正：ion host 的每个 socket 连接只读一行就关闭（subscribe 除外）。
// 所以网关要维护两类连接：
//   1. 一个长连接 SUB socket：发 {"method":"subscribe"} 走 subscribe_all，
//      持续收所有 worker 的事件流，转发给所有浏览器。
//   2. 每次浏览器 RPC 请求 → 临时开一个 socket 发命令、读响应、关闭。
//
// 用法：PORT=8080 node gateway.cjs

const http = require('http');
const fs = require('fs');
const path = require('path');
const net = require('net');
const crypto = require('crypto');

const SOCK = process.env.ION_HOST_SOCK || path.join(process.env.HOME, '.ion', 'host.sock');
const PORT = parseInt(process.env.PORT || '8080', 10);
const STATIC_DIR = __dirname;

// ─── WebSocket 帧编解码（同 v1）─────────────────────────────────────
const WS_MAGIC = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
function wsAccept(key) { return crypto.createHash('sha1').update(key + WS_MAGIC).digest('base64'); }
function parseFrame(buf) {
  if (buf.length < 2) return null;
  const b0 = buf[0], b1 = buf[1];
  const fin = (b0 & 0x80) !== 0, opcode = b0 & 0x0f, mask = (b1 & 0x80) !== 0;
  let len = b1 & 0x7f, offset = 2;
  if (len === 126) { if (buf.length < 4) return null; len = buf.readUInt16BE(2); offset = 4; }
  else if (len === 127) { if (buf.length < 10) return null; len = Number(buf.readBigUInt64BE(2)); offset = 10; }
  let maskKey = null;
  if (mask) { if (buf.length < offset + 4) return null; maskKey = buf.slice(offset, offset + 4); offset += 4; }
  if (buf.length < offset + len) return null;
  let payload = buf.slice(offset, offset + len);
  if (mask && maskKey) { const p = Buffer.allocUnsafe(len); for (let i = 0; i < len; i++) p[i] = payload[i] ^ maskKey[i % 4]; payload = p; }
  return { fin, opcode, payload, consumed: offset + len };
}
function encodeFrame(text) {
  const payload = Buffer.from(text, 'utf8'); const len = payload.length; let header;
  if (len < 126) { header = Buffer.allocUnsafe(2); header[1] = len; }
  else if (len < 65536) { header = Buffer.allocUnsafe(4); header[1] = 126; header.writeUInt16BE(len, 2); }
  else { header = Buffer.allocUnsafe(10); header[1] = 127; header.writeBigUInt64BE(BigInt(len), 2); }
  header[0] = 0x81;
  return Buffer.concat([header, payload]);
}

// ─── 浏览器连接管理 ─────────────────────────────────────────────────
const browsers = new Set(); // Set<net.Socket> 浏览器 WS 连接

function broadcastToBrowsers(obj) {
  const frame = encodeFrame(JSON.stringify(obj));
  for (const ws of browsers) {
    try { ws.write(frame); } catch {}
  }
}

// ─── SUB 长连接：订阅全局事件流 ─────────────────────────────────────
function startSubscriptionLoop() {
  const sub = net.createConnection(SOCK);
  sub.on('connect', () => {
    console.error('[gw] SUB socket connected, sending subscribe');
    sub.write(JSON.stringify({ method: 'subscribe' }) + '\n');
  });
  let buf = Buffer.alloc(0);
  sub.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    let nl;
    while ((nl = buf.indexOf(10)) >= 0) {
      const line = buf.slice(0, nl).toString('utf8').trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      let parsed;
      try { parsed = JSON.parse(line); } catch { continue; }
      // 把 ion 事件转发给所有浏览器
      broadcastToBrowsers(parsed);
    }
  });
  sub.on('error', (e) => console.error('[gw] SUB socket error:', e.message));
  sub.on('close', () => {
    console.error('[gw] SUB socket closed, reconnecting in 3s');
    setTimeout(startSubscriptionLoop, 3000);
  });
  return sub;
}

// ─── 按 session 订阅特定 worker 的事件流（instance subscribe）────────
// ion 的 worker 原始事件（text_delta / agent_start / agent_end / tool_*）
// 只能通过 subscribe --session <sid> 收到，subscribe_all 收不到。
// 所以 create_worker 拿到 sessionId 后，网关自动开一个 session SUB 连接。
const sessionSubs = new Map(); // sid → net.Socket
function subscribeSession(sid) {
  if (sessionSubs.has(sid)) return; // 已订阅
  const sub = net.createConnection(SOCK);
  sub.on('connect', () => {
    console.error(`[gw] SESSION SUB connected for ${sid}`);
    sub.write(JSON.stringify({ method: 'subscribe', session: sid }) + '\n');
  });
  let buf = Buffer.alloc(0);
  sub.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    let nl;
    while ((nl = buf.indexOf(10)) >= 0) {
      const line = buf.slice(0, nl).toString('utf8').trim();
      buf = buf.slice(nl + 1);
      if (!line) continue;
      let parsed;
      try { parsed = JSON.parse(line); } catch { continue; }
      // instance_event 格式：{ type: "instance_event", session, event: {...} }
      // 把里面的 event 提取出来，包成浏览器能理解的格式广播
      const evt = parsed.event || parsed;
      broadcastToBrowsers({ type: 'event', event: evt, session: sid });
    }
  });
  sub.on('error', (e) => console.error(`[gw] SESSION SUB ${sid} error:`, e.message));
  sub.on('close', () => {
    console.error(`[gw] SESSION SUB ${sid} closed`);
    sessionSubs.delete(sid);
  });
  sessionSubs.set(sid, sub);
}

// ─── RPC 短连接：发一条命令，读响应，关闭 ───────────────────────────
function rpc(method, params, session) {
  return new Promise((resolve, reject) => {
    const sock = net.createConnection(SOCK);
    let buf = Buffer.alloc(0);
    let done = false;
    const timeout = setTimeout(() => { if (!done) { done = true; try { sock.end(); } catch {} reject(new Error('rpc timeout 30s')); } }, 30000);
    sock.on('connect', () => {
      const req = { id: 'gw-' + Date.now(), method, params: params || {} };
      if (session) req.session = session;
      sock.write(JSON.stringify(req) + '\n');
    });
    sock.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      let nl;
      while ((nl = buf.indexOf(10)) >= 0) {
        const line = buf.slice(0, nl).toString('utf8').trim();
        buf = buf.slice(nl + 1);
        if (!line) continue;
        // 把事件也广播给浏览器（RPC 连接上可能也收到事件）
        try { broadcastToBrowsers(JSON.parse(line)); } catch {}
        // 找带 id 的响应
        try {
          const m = JSON.parse(line);
          if (m.id) {
            done = true;
            clearTimeout(timeout);
            try { sock.end(); } catch {}
            resolve(m);
            return;
          }
        } catch {}
      }
    });
    sock.on('error', (e) => { if (!done) { done = true; clearTimeout(timeout); reject(e); } });
    sock.on('close', () => { if (!done) { done = true; clearTimeout(timeout); resolve(null); } });
  });
}

// ─── HTTP + WebSocket 服务 ──────────────────────────────────────────
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'application/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml' };
const server = http.createServer((req, res) => {
  let urlPath = decodeURIComponent(req.url.split('?')[0]);
  res.setHeader('Access-Control-Allow-Origin', '*');

  // ── API: 读 session 历史消息 ──
  // GET /api/session/:sessionId → 从 JSONL 文件解析 user/assistant/tool 消息
  if (urlPath.startsWith('/api/session/')) {
    const sid = urlPath.replace('/api/session/', '');
    const ionDir = path.join(process.env.HOME, '.ion');
    const sessDir = path.join(ionDir, 'agent', 'sessions');
    // session_id 可能是 UUID（create_worker 返回的），也可能是 sess_xxx（文件名）。
    // 先按文件名找，再按内容里的 sessionId 匹配 UUID。
    fs.readdir(sessDir, (e, projects) => {
      if (e) { res.writeHead(500); res.end(JSON.stringify({error: e.message})); return; }
      let found = null;
      // 1. 直接按文件名找（sess_xxx.jsonl）
      for (const proj of projects) {
        const candidate = path.join(sessDir, proj, sid + '.jsonl');
        if (fs.existsSync(candidate)) { found = candidate; break; }
      }
      // 2. 按内容匹配（UUID → 找 JSONL 里 sessionId 字段等于 sid 的文件）
      if (!found) {
        for (const proj of projects) {
          const projDir = path.join(sessDir, proj);
          if (!fs.existsSync(projDir)) continue;
          for (const f of fs.readdirSync(projDir)) {
            if (!f.endsWith('.jsonl')) continue;
            const fp = path.join(projDir, f);
            try {
              // 只读前几行找 sessionId（header 或第一条消息）
              const fd = fs.openSync(fp, 'r');
              const buf = Buffer.alloc(2048);
              fs.readSync(fd, buf, 0, 2048, 0);
              fs.closeSync(fd);
              if (buf.toString('utf8').includes(sid)) { found = fp; break; }
            } catch {}
          }
          if (found) break;
        }
      }
      if (!found) { res.writeHead(404); res.end(JSON.stringify({error: 'session not found', sid, searched: projects.length + ' projects'})); return; }
      // 逐行解析 JSONL，提取消息
      const lines = fs.readFileSync(found, 'utf8').split('\n').filter(l => l.trim());
      const messages = [];
      for (const line of lines) {
        try {
          const m = JSON.parse(line);
          const role = m.role;
          if (role === 'user') {
            messages.push({ role: 'user', content: typeof m.content === 'string' ? m.content : JSON.stringify(m.content).slice(0, 500) });
          } else if (role === 'assistant') {
            const content = typeof m.content === 'string' ? m.content : (m.content?.[0]?.text || '');
            if (content) messages.push({ role: 'assistant', content: content.slice(0, 2000) });
            // tool calls
            if (m.tool_calls) {
              for (const tc of m.tool_calls) {
                messages.push({ role: 'tool', name: tc.function?.name || '?', content: tc.function?.arguments?.slice(0, 200) || '' });
              }
            }
          } else if (role === 'tool') {
            const content = typeof m.content === 'string' ? m.content : JSON.stringify(m.content);
            messages.push({ role: 'tool_result', content: content.slice(0, 300) });
          }
        } catch {}
      }
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ sid, messages, count: messages.length }));
    });
    return;
  }

  if (urlPath === '/') urlPath = '/index.html';
  const filePath = path.join(STATIC_DIR, path.normalize(urlPath).replace(/^(\.\.[/\\])+/, ''));
  if (!filePath.startsWith(STATIC_DIR)) { res.writeHead(403); res.end('forbidden'); return; }
  fs.readFile(filePath, (err, data) => {
    if (err) { res.writeHead(404); res.end('not found: ' + urlPath); return; }
    res.writeHead(200, { 'Content-Type': MIME[path.extname(filePath).toLowerCase()] || 'application/octet-stream' });
    res.end(data);
  });
});

server.on('upgrade', (req, socket) => {
  const key = req.headers['sec-websocket-key'];
  if (!key) { socket.destroy(); return; }
  socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + wsAccept(key) + '\r\n\r\n');
  browsers.add(socket);
  console.error(`[gw] browser connected (total ${browsers.size})`);

  let wsBuf = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    wsBuf = Buffer.concat([wsBuf, chunk]);
    let frame;
    while ((frame = parseFrame(wsBuf))) {
      wsBuf = wsBuf.slice(frame.consumed);
      if (frame.opcode === 0x8) { try { socket.end(); } catch {} return; }
      if (frame.opcode !== 0x1) continue;
      const text = frame.payload.toString('utf8');
      let msg; try { msg = JSON.parse(text); } catch { continue; }
      // 浏览器的 RPC 请求 → 开短连接发
      if (msg.type === 'rpc' || msg.method) {
        console.error(`[gw] RPC: ${msg.method}`);
        rpc(msg.method, msg.params || {}, msg.session)
          .then((resp) => {
            if (resp) {
              try { socket.write(encodeFrame(JSON.stringify(resp))); } catch {}
              // create_worker 成功后，拿 sessionId 自动订阅该 worker 的事件流
              // （ion 的 text_delta 等只通过 subscribe --session 推送，全局订阅收不到）
              if (msg.method === 'create_worker' && resp.data && resp.data.sessionId) {
                subscribeSession(resp.data.sessionId);
              }
            }
          })
          .catch((e) => { try { socket.write(encodeFrame(JSON.stringify({ type: 'rpc_error', method: msg.method, message: e.message }))); } catch {} });
      }
    }
  });
  const cleanup = () => { browsers.delete(socket); console.error(`[gw] browser disconnected (total ${browsers.size})`); };
  socket.on('error', cleanup);
  socket.on('close', cleanup);
});

server.listen(PORT, '0.0.0.0', () => {
  console.error(`ION Dashboard 网关 v2 已启动:`);
  console.error(`  HTTP:  http://localhost:${PORT}/`);
  console.error(`  ion sock: ${SOCK}`);
  console.error(`  架构: 1 SUB 长连接 + N RPC 短连接`);
  startSubscriptionLoop();
});
