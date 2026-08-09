#!/usr/bin/env node
// ION Dashboard WebSocket 网关
// 把浏览器的 WebSocket 连接桥接到 ion serve 的 Unix socket。
//
// 协议：
//   浏览器 → 网关（WebSocket 消息）：{"type":"rpc","method":"...","params":{...}}
//   网关 → 浏览器：ion socket 的每一行 JSON 原样转发（事件 + 响应）
//
// 还 serve 静态文件（dashboard/index.html）供浏览器加载。
//
// 用法：node gateway.js [--sock ~/.ion/host.sock] [--port 8080]
// 无外部依赖，纯 Node 内置模块。

const http = require('http');
const fs = require('fs');
const path = require('path');
const net = require('net');
const crypto = require('crypto');

const SOCK = process.env.ION_HOST_SOCK || '~/.ion/host.sock'.replace('~', process.env.HOME);
const PORT = parseInt(process.env.PORT || '8080', 10);
const STATIC_DIR = __dirname; // dashboard/ 目录，含 index.html

// ─── 极简 WebSocket 实现（Node 内置，不装 ws 库）──────────────────────
const WS_MAGIC = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

function wsAccept(key) {
  return crypto.createHash('sha1').update(key + WS_MAGIC).digest('base64');
}

// 解析一个 WebSocket 帧（支持文本帧 + mask）。返回 {payload, opcode} 或 null（数据不够）。
function parseFrame(buf) {
  if (buf.length < 2) return null;
  const b0 = buf[0], b1 = buf[1];
  const fin = (b0 & 0x80) !== 0;
  const opcode = b0 & 0x0f;
  let mask = (b1 & 0x80) !== 0;
  let len = b1 & 0x7f;
  let offset = 2;
  if (len === 126) { if (buf.length < 4) return null; len = buf.readUInt16BE(2); offset = 4; }
  else if (len === 127) { if (buf.length < 10) return null; len = Number(buf.readBigUInt64BE(2)); offset = 10; }
  let maskKey = null;
  if (mask) { if (buf.length < offset + 4) return null; maskKey = buf.slice(offset, offset + 4); offset += 4; }
  if (buf.length < offset + len) return null;
  let payload = buf.slice(offset, offset + len);
  if (mask && maskKey) {
    const p = Buffer.allocUnsafe(len);
    for (let i = 0; i < len; i++) p[i] = payload[i] ^ maskKey[i % 4];
    payload = p;
  }
  return { fin, opcode, payload, consumed: offset + len };
}

// 编码一个 WebSocket 文本帧（服务端→客户端，不 mask）。
function encodeFrame(text) {
  const payload = Buffer.from(text, 'utf8');
  const len = payload.length;
  let header;
  if (len < 126) {
    header = Buffer.allocUnsafe(2);
    header[1] = len;
  } else if (len < 65536) {
    header = Buffer.allocUnsafe(4);
    header[1] = 126; header.writeUInt16BE(len, 2);
  } else {
    header = Buffer.allocUnsafe(10);
    header[1] = 127; header.writeBigUInt64BE(BigInt(len), 2);
  }
  header[0] = 0x81; // FIN + text
  return Buffer.concat([header, payload]);
}

// ─── 连 ion socket 的辅助：发一条 RPC，返回 socket（持续读事件转发）───
// 每个浏览器 WS 连接独占一个 ion socket 连接，便于把事件流分流给对应的浏览器。
function connectIonSock(onLine, onError, onClose) {
  const sock = net.createConnection(SOCK);
  let buf = Buffer.alloc(0);
  sock.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    let nl;
    while ((nl = buf.indexOf(10)) >= 0) { // 按 \n 分行
      const line = buf.slice(0, nl).toString('utf8').trim();
      buf = buf.slice(nl + 1);
      if (line) onLine(line);
    }
  });
  sock.on('error', onError);
  sock.on('close', onClose);
  return sock;
}

// ─── HTTP 服务：serve 静态 HTML + 升级 WebSocket ──────────────────────
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'application/javascript', '.css': 'text/css', '.png': 'image/png', '.svg': 'image/svg+xml' };

const server = http.createServer((req, res) => {
  // 简单 CORS + 首页指向 index.html
  res.setHeader('Access-Control-Allow-Origin', '*');
  let urlPath = decodeURIComponent(req.url.split('?')[0]);
  if (urlPath === '/') urlPath = '/index.html';
  // 防目录穿越
  const filePath = path.join(STATIC_DIR, path.normalize(urlPath).replace(/^(\.\.[/\\])+/, ''));
  if (!filePath.startsWith(STATIC_DIR)) { res.writeHead(403); res.end('forbidden'); return; }
  fs.readFile(filePath, (err, data) => {
    if (err) { res.writeHead(404); res.end('not found: ' + urlPath); return; }
    const ext = path.extname(filePath).toLowerCase();
    res.writeHead(200, { 'Content-Type': MIME[ext] || 'application/octet-stream' });
    res.end(data);
  });
});

// WebSocket 升级
server.on('upgrade', (req, socket) => {
  const key = req.headers['sec-websocket-key'];
  if (!key) { socket.destroy(); return; }
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n' +
    'Upgrade: websocket\r\n' +
    'Connection: Upgrade\r\n' +
    'Sec-WebSocket-Accept: ' + wsAccept(key) + '\r\n\r\n'
  );

  // 为这个浏览器连接开一个 ion socket
  let ionSock = null;
  let ionBuf = Buffer.alloc(0);

  const sendToBrowser = (obj) => {
    socket.write(encodeFrame(JSON.stringify(obj)));
  };

  ionSock = connectIonSock(
    (line) => {
      console.error(`[gw] ion→ws line: ${line.slice(0, 120)}`);
      // ion 的每一行 JSON 原样转发给浏览器
      let parsed;
      try { parsed = JSON.parse(line); } catch { sendToBrowser({ type: 'raw', line }); return; }
      sendToBrowser(parsed);
    },
    (err) => { console.error(`[gw] ion sock error: ${err.message}`); sendToBrowser({ type: 'ion_error', message: err.message }); },
    () => { console.error('[gw] ion sock closed'); sendToBrowser({ type: 'ion_closed' }); }
  );
  ionSock.on('connect', () => console.error('[gw] ion socket connected'));

  // 处理浏览器发来的消息
  let wsBuf = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    wsBuf = Buffer.concat([wsBuf, chunk]);
    let frame;
    while ((frame = parseFrame(wsBuf))) {
      wsBuf = wsBuf.slice(frame.consumed);
      if (frame.opcode === 0x8) { // close
        try { ionSock.end(); } catch {}
        socket.end();
        return;
      }
      if (frame.opcode !== 0x1) continue; // 只处理文本帧
      const text = frame.payload.toString('utf8');
      console.error(`[gw] ws→ion text: ${text.slice(0, 120)}`);
      let msg;
      try { msg = JSON.parse(text); } catch { sendToBrowser({ type: 'error', message: 'invalid json from browser' }); continue; }

      // 收到浏览器的 RPC 请求 → 转成 ion socket 格式写入
      if (msg.type === 'rpc' || msg.method) {
        const id = msg.id || ('ws-' + Date.now());
        const req = { id, method: msg.method, params: msg.params || {} };
        if (msg.session) req.session = msg.session;
        const wrote = ionSock.write(JSON.stringify(req) + '\n');
        console.error(`[gw] forwarded to ion (method=${msg.method}, wrote=${wrote})`);
        sendToBrowser({ type: 'rpc_sent', id, method: msg.method });
      } else if (msg.type === 'subscribe') {
        // 发一个 subscribe 命令让 ion 推事件流
        const id = 'ws-' + Date.now();
        ionSock.write(JSON.stringify({ id, method: 'subscribe', params: {} }) + '\n');
      }
    }
  });

  socket.on('error', () => { try { ionSock.end(); } catch {} });
  socket.on('close', () => { try { ionSock.end(); } catch {} });
});

server.listen(PORT, '0.0.0.0', () => {
  console.log(`ION Dashboard 网关已启动:`);
  console.log(`  HTTP:     http://localhost:${PORT}/`);
  console.log(`  WebSocket: ws://localhost:${PORT}/ (浏览器自动连)`);
  console.log(`  ion sock: ${SOCK}`);
  console.log(`  静态目录: ${STATIC_DIR}`);
  console.log(`按 Ctrl+C 退出。`);
});
