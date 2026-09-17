#!/usr/bin/env python3
# approval_push_bridge.py — ION 统一审批总线 → 手机推送桥（v1）
#
# 订阅 host 的 UI 事件流（subscribe {ui:true}），把 ApprovalRequest（统一审批
# 总线事件，APPROVAL_BUS.md §5）推送到手机推送网关（webhook）。
#
# ══ 部署方法 ══
#
#   # 1. 两个环境变量（webhook 由运行时注入，绝不硬编码进仓库）：
#   export ION_APPROVAL_WEBHOOK='https://your-push-gateway.example/push'  # 必填
#   export ION_APPROVAL_BRIDGE_LOG=/tmp/approval_bridge.log               # 可选，默认即此
#   export ION_HOST_SOCKET=~/.ion/host.sock                               # 可选，默认即此
#
#   # 2. nohup 常驻（生产 host 在跑时启动；断线自动重连，指数退避 1s→30s）：
#   nohup python3 scripts/approval_push_bridge.py >/dev/null 2>&1 &
#
#   # 3. 看日志（一行一事件含 ts；dedupe 命中也会记录）：
#   tail -f /tmp/approval_bridge.log
#
# ══ 人工验证 / v2 探针 ══
#
#   # 发一条标题含 TEST 的样例推送，验证手机可达（不连 host、不发订阅）：
#   python3 scripts/approval_push_bridge.py --test-push
#
#   # v2 路线探针：额外附 "url" 字段——探测推送网关是否支持可点击链接/动作按钮
#   # （支持的话 v2 可以直接在通知上挂 webui 审批按钮，点开即审）：
#   python3 scripts/approval_push_bridge.py --test-push --with-url
#
# ══ 自测（CI 用，不起真实网络/不碰真实 ~/.ion）══
#
#   python3 scripts/approval_push_bridge.py --selftest
#   # 本地起 mock unix socket（按协议吐 hello/ack/3 条事件帧，其中 1 条重复）
#   # + mock HTTP webhook（落盘收到的 POST），跑完整链路后断言：
#   #   恰好 2 条推送（重复帧被 30s 冷却吞掉）、payload 形状正确。
#   # 退出码 0=通过 / 1=失败。
#
# 协议依据：docs/design/APPROVAL_BUS.md（§5 事件规格）+
#          docs/design/SUBSCRIBE_PROTOCOL.md（§2 帧规格）。
# 总线统一事件判据：data.approval 对象存在（worker 原生 file-approval 事件
# 无此键，被过滤）；条目取 data.approval，缺失时回落 data 平铺契约字段。
# 纯标准库（python3，无第三方依赖）。
#
# 应答链路（手机上收到推送后）：
#   ion approvals approve <id>   # 或 reject；webui 按钮走 approval_respond RPC

import argparse
import html as _html
import secrets
import hashlib
import json
import os
import socket
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer, ThreadingHTTPServer

DEFAULT_SOCKET = os.path.expanduser("~/.ion/host.sock")
DEFAULT_LOG = "/tmp/approval_bridge.log"
COOLDOWN_SECONDS = 30.0
HANDSHAKE_TIMEOUT = 3.0

KIND_LABELS = {
    "file_snapshot": "写文件审批",
    "ui_ask": "权限询问",
    "remote_verb": "远程动词",
}
# level：file_snapshot/remote_verb 时间敏感；ui_ask 有 120s 窗口但属警告级
KIND_LEVELS = {
    "file_snapshot": "time-sensitive",
    "remote_verb": "time-sensitive",
    "ui_ask": "warning",
}


def ts() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def log_line(log_path: str, msg: str) -> None:
    """一行一事件追加日志（含 ts）。日志失败静默（推送优先于日志）。"""
    try:
        with open(log_path, "a", encoding="utf-8") as f:
            f.write(f"{ts()} {msg}\n")
    except OSError:
        pass


# ---------------------------------------------------------------------------
# 帧解析：从 subscribe(ui) 流的帧里提取总线 ApprovalRequest 条目
# ---------------------------------------------------------------------------

def extract_approval(frame):
    """识别总线统一审批事件，返回扁平条目 dict；非审批事件返回 None。

    兼容三种帧形（SUBSCRIBE_PROTOCOL.md §2）：
      1. ui 流：        {"type":"ui_event","ui_type":"ApprovalRequest",...,"data":{...}}
      2. extension 流：  {"type":"extension_event","customType":"ApprovalRequest",...}
      3. worker 原始壳： {"type":"event","event":{"type":"extension_event",...}}
    """
    if not isinstance(frame, dict):
        return None
    ev = None
    if frame.get("type") == "ui_event" and frame.get("ui_type") == "ApprovalRequest":
        ev = frame
    elif frame.get("type") == "extension_event" and frame.get("customType") == "ApprovalRequest":
        ev = frame
    elif frame.get("type") == "event":
        inner = frame.get("event")
        if (isinstance(inner, dict)
                and inner.get("type") == "extension_event"
                and inner.get("customType") == "ApprovalRequest"):
            ev = inner
    if ev is None:
        return None
    data = ev.get("data")
    if not isinstance(data, dict):
        return None
    # 总线统一事件判据：data.approval 存在（APPROVAL_BUS.md §5）。
    # worker 原生 file-approval 事件（ext=file-approval，data 含 files/requestId，
    # 无 kind 无 approval）在此被过滤——桥只推统一 id（apr_ 前缀）。
    entry = data.get("approval")
    if isinstance(entry, dict) and entry.get("id"):
        src = entry
    elif data.get("id") and data.get("kind"):
        src = data  # 兼容：总线事件 data 顶层平铺契约字段（approval 键缺失时）
    else:
        return None
    sid = str(src.get("sessionId") or "")
    return {
        "id": str(src.get("id") or ""),
        "kind": str(src.get("kind") or ""),
        "summary": str(src.get("summary") or ""),
        "sessionId": sid,
        "raisedAtMs": src.get("raisedAtMs"),
    }


def build_payload(entry: dict, page_url: str | None = None) -> dict:
    """ApprovalEntry → 推送网关 payload（v2：page_url 存在时附可点击审批页链接）。"""
    kind = entry.get("kind", "")
    label = KIND_LABELS.get(kind, kind or "未知类型")
    sid = entry.get("sessionId", "")
    short = sid[-8:] if len(sid) > 8 else sid
    lines = [f"**{label}**"]
    if entry.get("summary"):
        lines.append(str(entry["summary"]))
    if short:
        lines.append(f"会话 `{short}`")
    aid = entry.get("id", "")
    lines.append(f"应答：`ion approvals approve {aid}`（或 webui 审批按钮）")
    if kind == "ui_ask":
        lines.append("⏳ 120s 内有效")
    payload = {
        "title": f"🔴审批待处理 {aid}",
        "body": "\n".join(lines),
        "markdown": "true",
        "level": KIND_LEVELS.get(kind, "warning"),
        "group": "ion-approvals",
    }
    if page_url:
        payload["url"] = page_url
    return payload


def post_webhook(url: str, payload: dict, timeout: float = 10.0):
    """POST JSON 到推送网关。返回 (http_status|None, body_text)。"""
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(
        url, data=data,
        headers={
            "Content-Type": "application/json; charset=utf-8",
            # Cloudflare 类网关会封 python 默认 UA（实测 drel 403 error 1010）
            "User-Agent": "ion-approval-bridge/1.0",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read(4096).decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        try:
            body = e.read(4096).decode("utf-8", "replace")
        except Exception:
            body = ""
        return e.code, body
    except Exception as e:  # 网络层失败（DNS/拒连/超时）
        return None, str(e)


# ---------------------------------------------------------------------------
# v2：本机 HTTP 审批页——推送 url 指向 /p/<token>，手机点开即批准/拒绝
# ---------------------------------------------------------------------------

PAGE_TTL_SECONDS = 1800.0
_PAGES: dict[str, dict] = {}
_PAGES_LOCK = threading.Lock()
_RPC_SOCK = {"path": ""}
_LOG = {"path": ""}
PAGE_HTTPD = None
PAGE_PORT = 0


def lan_ip() -> str:
    """探测本机局域网 IP（UDP connect 惯用法，不真正发包）。"""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("10.255.255.255", 1))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"


def register_page_token(entry: dict) -> str:
    """审批条目 → 一次性页面 token（30 分钟有效，用后即焚）。"""
    token = secrets.token_hex(16)
    with _PAGES_LOCK:
        now = time.monotonic()
        for t in [t for t, v in _PAGES.items() if now - v["exp_start"] > PAGE_TTL_SECONDS]:
            _PAGES.pop(t, None)
        _PAGES[token] = {
            "id": str(entry.get("id", "")),
            "label": KIND_LABELS.get(entry.get("kind", ""), str(entry.get("kind", "")) or "未知类型"),
            "summary": str(entry.get("summary", "")),
            "exp_start": now,
        }
    return token


def _take_page(token: str) -> dict | None:
    with _PAGES_LOCK:
        v = _PAGES.pop(token, None)
    if v is None or time.monotonic() - v["exp_start"] > PAGE_TTL_SECONDS:
        return None
    return v


def rpc_once(method: str, params: dict, timeout: float = 8.0):
    """对 host socket 的一次性 RPC。返回 (success, data_or_error_text)。"""
    path = _RPC_SOCK["path"] or DEFAULT_SOCKET
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(timeout)
        s.connect(path)
        rid = "rpc" + secrets.token_hex(4)
        s.sendall((json.dumps({"id": rid, "method": method, "params": params}) + "\n").encode())
        deadline = time.monotonic() + timeout
        s.settimeout(timeout)
        while time.monotonic() < deadline:
            line = s.makefile("r", encoding="utf-8", newline="\n").readline()
            if not line:
                break
            try:
                v = json.loads(line.strip())
            except ValueError:
                continue
            if v.get("id") == rid:
                return bool(v.get("success")), v.get("data") if v.get("success") else str(v.get("error", ""))
        return False, "host 未在超时内应答"
    except OSError as e:
        return False, f"{type(e).__name__}: {e}"
    finally:
        try:
            s.close()
        except Exception:
            pass


def _page_html(title: str, body_html: str) -> bytes:
    return (f"<!doctype html><html><head><meta charset=\"utf-8\">"
            f"<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">"
            f"<title>{_html.escape(title)}</title>"
            f"<style>body{{font-family:-apple-system,sans-serif;max-width:640px;margin:32px auto;"
            f"padding:0 16px}}.btn{{display:inline-block;padding:14px 28px;margin:8px 12px 8px 0;"
            f"border-radius:10px;text-decoration:none;font-size:17px}}"
            f".ok{{background:#1a7f37;color:#fff}}.no{{background:#c62828;color:#fff}}"
            f"pre{{background:#f4f4f4;padding:10px;border-radius:8px;white-space:pre-wrap}}</style>"
            f"</head><body>{body_html}</body></html>").encode("utf-8")


class _ApprovalPageHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        parts = self.path.strip("/").split("/")
        if len(parts) < 2 or parts[0] != "p":
            self._send(404, _page_html("未找到", "<p>路径无效</p>"))
            return
        token = parts[1]
        decision = parts[2] if len(parts) > 2 else ""
        if decision not in ("", "approve", "reject"):
            self._send(404, _page_html("未找到", "<p>路径无效</p>"))
            return
        info = _take_page(token) if decision else (lambda: (
            (lambda v: v if v is not None and time.monotonic() - v["exp_start"] <= PAGE_TTL_SECONDS else None)(
                _PAGES.get(token))))()
        if info is None:
            self._send(404, _page_html("链接无效", "<p>该审批链接不存在、已使用或已过期。</p>"))
            return
        esc = _html.escape
        if decision == "":
            body = (f"<h2>🔴 {esc(info['label'])}</h2>"
                    f"<pre>{esc(info['summary'])}</pre>"
                    f"<p>审批 ID：<code>{esc(info['id'])}</code></p>"
                    f"<a class=\"btn ok\" href=\"/p/{token}/approve\">✔ 批准</a>"
                    f"<a class=\"btn no\" href=\"/p/{token}/reject\">✘ 拒绝</a>"
                    f"<p style=\"color:#888\">链接一次性有效（防误触后不可回退）</p>")
            self._send(200, _page_html(f"审批 {info['id']}", body))
            return
        ok, data = rpc_once("approval_respond", {
            "id": info["id"],
            "decision": "approve" if decision == "approve" else "reject",
        })
        log_line(_LOG["path"], f"[page] {decision} {info['id']} rpc_ok={ok}")
        if ok:
            remain = ""
            if isinstance(data, dict) and "remaining" in data:
                remain = f"（剩余待审批 {data['remaining']} 条）"
            verb = "已批准" if decision == "approve" else "已拒绝"
            body = f"<h2>{'✔' if decision == 'approve' else '🚫'} {verb} {esc(info['id'])}</h2><p>{esc(verb)}{remain}</p>"
            self._send(200, _page_html(verb, body))
        else:
            body = (f"<h2>✘ 操作失败</h2><pre>{esc(str(data))}</pre>"
                    f"<p>该链接已消耗；若审批仍待处理请用 ion approvals 命令或 webui 重试。</p>")
            self._send(200, _page_html("操作失败", body))

    def _send(self, code: int, body: bytes):
        self.send_response(code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        pass


def start_page_server(sock_path: str, log_path: str, port: int = 0,
                      host: str = "0.0.0.0") -> int:
    """启动审批页 HTTP 服务（daemon 线程）。port=0 → 随机端口。返回实际端口。"""
    global PAGE_HTTPD, PAGE_PORT
    _RPC_SOCK["path"] = sock_path
    _LOG["path"] = log_path
    PAGE_HTTPD = ThreadingHTTPServer((host, port), _ApprovalPageHandler)
    PAGE_PORT = PAGE_HTTPD.server_address[1]
    threading.Thread(target=PAGE_HTTPD.serve_forever, daemon=True).start()
    return PAGE_PORT


# ---------------------------------------------------------------------------
# 主循环：连接 → 握手 → 订阅 → 读帧 → 冷却去重 → 推送 → 断线重连
# ---------------------------------------------------------------------------

def dedupe_key(entry: dict) -> str:
    digest = hashlib.sha1(entry.get("summary", "").encode("utf-8")).hexdigest()[:16]
    return f"{entry.get('kind', '')}:{digest}"


def run_bridge(sock_path: str, webhook_url: str, log_path: str,
               stop_after_events: int | None = None, http_port: int = 0) -> int:
    """长驻主循环。stop_after_events 仅 selftest 用：处理满 N 条审批事件后正常返回。
    http_port > 0 时本机起审批页（推送附可点击 url，手机直达批准/拒绝）。"""
    page_base = None
    if http_port != 0:
        real_port = start_page_server(sock_path, log_path, port=http_port)
        page_base = f"http://{lan_ip()}:{real_port}/p/"
        log_line(log_path, f"[page] approval page serving at {page_base}<token>")
    backoff = 1.0
    processed = 0
    last_push: dict[str, float] = {}  # dedupe_key -> monotonic ts
    while True:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(sock_path)
            sock.settimeout(HANDSHAKE_TIMEOUT)
            reader = sock.makefile("r", encoding="utf-8", newline="\n")

            # 握手 1：hello（拿 protocolVersion/hostId；不匹配场景 v1 只记录）
            sock.sendall(b'{"id":"b1","method":"hello"}\n')
            hello_ok = False
            deadline = time.monotonic() + HANDSHAKE_TIMEOUT
            while time.monotonic() < deadline:
                line = reader.readline()
                if not line:
                    break
                try:
                    v = json.loads(line.strip())
                except ValueError:
                    continue
                if v.get("id") == "b1" and v.get("type") == "response":
                    hello_ok = bool(v.get("success"))
                    if hello_ok:
                        d = v.get("data") or {}
                        log_line(log_path, f"[hello] ok protocolVersion="
                                 f"{d.get('protocolVersion')} hostId={d.get('hostId')}")
                    break
            if not hello_ok:
                log_line(log_path, "[hello] no/failed reply within "
                         f"{HANDSHAKE_TIMEOUT:.0f}s, continuing anyway")

            # 握手 2：订阅 UI 事件流。
            # 🔧 帧形依据 SUBSCRIBE_PROTOCOL.md §2：ui 标志在请求**顶层**
            # （{"method":"subscribe","ui":true}），host 读 cmd["ui"]；
            # 放 params 里会被路由到 extension 订阅分支（收不到 ui 路由事件）。
            sock.sendall(b'{"id":"b2","method":"subscribe","ui":true}\n')
            log_line(log_path, "[subscribe] ui stream requested")
            sock.settimeout(None)  # 生产态阻塞读（EOF/异常驱动重连）
            backoff = 1.0

            while True:
                line = reader.readline()
                if not line:  # EOF → 断线
                    raise ConnectionError("host closed connection")
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    frame = json.loads(stripped)
                except ValueError:
                    log_line(log_path, f"[warn] non-JSON frame: {stripped[:120]}")
                    continue
                # 订阅 ack 记录（{"type":"subscribed","stream":"ui"}）
                if frame.get("type") == "subscribed":
                    log_line(log_path, f"[subscribe] ack stream={frame.get('stream')}")
                    continue
                entry = extract_approval(frame)
                if entry is None:
                    continue

                processed += 1
                key = dedupe_key(entry)
                now = time.monotonic()
                last = last_push.get(key)
                if last is not None and (now - last) < COOLDOWN_SECONDS:
                    log_line(log_path, f"[dedupe] {entry['id']} kind={entry['kind']} "
                             f"same (kind+summary) within {COOLDOWN_SECONDS:.0f}s, skipped")
                else:
                    last_push[key] = now
                    page_url = (page_base + register_page_token(entry)) if page_base else None
                    payload = build_payload(entry, page_url=page_url)
                    status, body = post_webhook(webhook_url, payload)
                    log_line(log_path, f"[pushed] {entry['id']} kind={entry['kind']} "
                             f"session={entry['sessionId']} webhook={status}")
                    if status is None or not (200 <= status < 300):
                        log_line(log_path, f"[push-fail] status={status} body={body[:200]}")
                if stop_after_events is not None and processed >= stop_after_events:
                    return 0
        except FileNotFoundError:
            log_line(log_path, f"[reconnect] socket not found: {sock_path}")
        except (ConnectionError, BrokenPipeError, OSError) as e:
            log_line(log_path, f"[reconnect] {type(e).__name__}: {e}")
        finally:
            try:
                sock.close()
            except Exception:
                pass
        if stop_after_events is not None:
            # selftest 模式下不重连（mock server 已关）
            return 1
        log_line(log_path, f"[reconnect] retry in {backoff:.0f}s")
        time.sleep(backoff)
        backoff = min(backoff * 2, 30.0)


# ---------------------------------------------------------------------------
# --test-push：样例推送（人工验证手机可达 / v2 url 探针）
# ---------------------------------------------------------------------------

def cmd_test_push(webhook_url: str, with_url: bool, sock_path: str = "",
                  serve_minutes: float = 10.0) -> int:
    entry = {
        "id": "apr_testpush1",
        "kind": "ui_ask",
        "summary": "TEST 样例推送（approval bridge probe）— 点链接可体验完整审批页",
        "sessionId": "sess_testpush01",
        "raisedAtMs": int(time.time() * 1000),
    }
    page_url = None
    if with_url:
        port = start_page_server(sock_path or DEFAULT_SOCKET, DEFAULT_LOG)
        page_url = f"http://{lan_ip()}:{port}/p/{register_page_token(entry)}"
    payload = build_payload(entry, page_url=page_url)
    payload["title"] = f"🔴审批待待处理 TEST {entry['id']}"
    payload["title"] = f"🔴审批待处理 TEST {entry['id']}"
    status, body = post_webhook(webhook_url, payload)
    if status is not None and 200 <= status < 300:
        print(f"✔ 测试推送成功 (HTTP {status})")
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        if page_url and serve_minutes > 0:
            print(f"ℹ 手机点开通知 → 审批页（批准/拒绝按钮）。TEST 的 id 在总线中不存在，"
                  f"点击按钮应看到诚实的失败页——这正好验证全链路。"
                  f"页面服务 {serve_minutes:.0f} 分钟后自动退出（Ctrl-C 提前结束）。")
            try:
                time.sleep(serve_minutes * 60)
            except KeyboardInterrupt:
                pass
        return 0
    print(f"✘ 测试推送失败 status={status} body={body}", file=sys.stderr)
    return 1


# ---------------------------------------------------------------------------
# --selftest：mock 全链（mock unix socket + mock HTTP webhook），CI 靠它
# ---------------------------------------------------------------------------

MOCK_FRAMES_SPEC = [
    # (apr_id, kind, summary, session)——第 3 条与第 1 条同 kind+summary（重复帧）
    # session 短码 = 最后 8 位（sess_bridge01 → bridge01）
    ("apr_deadbeef", "file_snapshot", "1 file(s) pending review: bus_a.txt", "sess_bridge01"),
    ("apr_cafef00d", "ui_ask", "Ask: 允许执行 bash?", "sess_bridge01"),
    ("apr_deadbeef", "file_snapshot", "1 file(s) pending review: bus_a.txt", "sess_bridge01"),
]


def bus_frame(apr_id: str, kind: str, summary: str, session: str) -> dict:
    """按 SUBSCRIBE_PROTOCOL.md §1.2 + APPROVAL_BUS.md §5 的真实帧形构造。"""
    entry = {
        "id": apr_id, "kind": kind, "sessionId": session, "summary": summary,
        "payload": {"nativeRequestId": f"req_{apr_id}"}, "raisedAtMs": int(time.time() * 1000),
    }
    return {
        "type": "ui_event", "ui_type": "ApprovalRequest", "extension": "host",
        "session": session, "route": "ui",
        "data": {**entry, "approval": entry},
    }


class _MockWebhookHandler(BaseHTTPRequestHandler):
    dump_path = None  # 类属性：selftest 注入

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8", "replace")
        try:
            with open(self.dump_path, "a", encoding="utf-8") as f:
                f.write(body.replace("\n", " ") + "\n")
        except OSError:
            pass
        resp = json.dumps({"ok": True}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, fmt, *args):  # 静默 http.server 默认 stderr 日志
        pass


def mock_webhook_server(dump_path: str):
    """起 mock HTTP webhook（127.0.0.1 随机端口），返回 (server, url)。"""
    _MockWebhookHandler.dump_path = dump_path
    server = HTTPServer(("127.0.0.1", 0), _MockWebhookHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, f"http://127.0.0.1:{server.server_address[1]}/push"


def mock_host_server(sock_path: str, ready: threading.Event):
    """起 mock unix socket：按协议回 hello/subscribe ack，再吐 3 条事件帧
    （两条唯一 + 一条重复）。daemon 线程；selftest 结束随进程退出。"""

    def handle_conn(conn):
        try:
            conn.settimeout(10)
            reader = conn.makefile("r", encoding="utf-8", newline="\n")
            while True:
                line = reader.readline()
                if not line:
                    break
                try:
                    req = json.loads(line.strip())
                except ValueError:
                    continue
                method = req.get("method")
                if method == "hello":
                    conn.sendall((json.dumps({
                        "type": "response", "id": req.get("id", "b1"), "success": True,
                        "data": {"protocolVersion": 1, "hostId": "selftest-mock-host"},
                    }) + "\n").encode())
                elif method == "approval_respond":
                    conn.sendall((json.dumps({
                        "type": "response", "id": req.get("id", "r1"), "success": True,
                        "data": {"id": (req.get("params") or {}).get("id"),
                                 "kind": "file_snapshot", "decision":
                                 (req.get("params") or {}).get("decision"), "remaining": 0},
                    }) + "\n").encode())
                elif method == "subscribe":
                    conn.sendall(b'{"type":"subscribed","stream":"ui"}\n')
                    for spec in MOCK_FRAMES_SPEC:
                        time.sleep(0.3)
                        conn.sendall((json.dumps(bus_frame(*spec)) + "\n").encode())
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def serve():
        try:
            if os.path.exists(sock_path):
                os.unlink(sock_path)
            srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            srv.bind(sock_path)
            srv.listen(4)
            ready.set()
            while True:
                conn, _ = srv.accept()
                threading.Thread(target=handle_conn, args=(conn,), daemon=True).start()
        except OSError:
            pass
        finally:
            ready.set()

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return t


def cmd_selftest() -> int:
    import tempfile
    import shutil

    tmp = tempfile.mkdtemp(prefix="ion-apr-bridge-selftest-")
    failures: list[str] = []
    try:
        dump = os.path.join(tmp, "webhook_dump.jsonl")
        sock_path = os.path.join(tmp, "mock.sock")
        log_path = os.path.join(tmp, "bridge.log")
        webhook_url = None

        server, webhook_url = mock_webhook_server(dump)
        ready = threading.Event()
        mock_host_server(sock_path, ready)
        if not ready.wait(timeout=5):
            print("✘ selftest: mock host 未就绪", file=sys.stderr)
            return 1

        # 自测用空闲端口起审批页（run_bridge 的 http_port=0 语义是关闭）
        _probe = socket.socket()
        _probe.bind(("127.0.0.1", 0))
        free_port = _probe.getsockname()[1]
        _probe.close()
        rc = run_bridge(sock_path, webhook_url, log_path, stop_after_events=3, http_port=free_port)
        server.shutdown()
        if rc != 0:
            failures.append(f"run_bridge 返回 {rc}（预期 0）")
            return finish(failures, tmp, log_path, dump)

        pushes = []
        try:
            with open(dump, encoding="utf-8") as f:
                pushes = [json.loads(l) for l in f if l.strip()]
        except (OSError, ValueError) as e:
            failures.append(f"webhook 落盘读取失败: {e}")
        log_text = ""
        try:
            with open(log_path, encoding="utf-8") as f:
                log_text = f.read()
        except OSError as e:
            failures.append(f"bridge log 读取失败: {e}")

        # 断言 1：恰 2 条推送（重复帧被冷却吞）
        if len(pushes) == 2:
            pass
        else:
            failures.append(f"推送条数={len(pushes)}（预期 2：重复帧应被 30s 冷却吞掉）")
        # 断言 2：冷却日志留痕
        if "[dedupe]" in log_text:
            pass
        else:
            failures.append("日志无 [dedupe] 记录（重复帧未走冷却路径）")
        # 断言 3：握手/订阅链路
        if "[hello] ok" in log_text and "stream=ui" in log_text:
            pass
        else:
            failures.append("握手/订阅 ack 未按协议完成")

        # 断言 4：payload 形状（两条各验）
        def has_all(p, checks):
            return all(c[0] in p and c[1] in str(p.get(c[0], "")) for c in checks)

        if len(pushes) >= 1:
            p0 = pushes[0]
            shape0 = [
                ("title", "🔴审批待处理 apr_deadbeef"),
                ("markdown", "true"),
                ("level", "time-sensitive"),
                ("group", "ion-approvals"),
                ("body", "写文件审批"),
                ("body", "1 file(s) pending review: bus_a.txt"),
                ("body", "bridge01"),  # sessionId 短码（sess_bridge01 → bridge01）
                ("body", "ion approvals approve apr_deadbeef"),
            ]
            if has_all(p0, shape0):
                pass
            else:
                failures.append(f"payload#1 形状不符: {json.dumps(p0, ensure_ascii=False)}")
        if len(pushes) >= 2:
            p1 = pushes[1]
            shape1 = [
                ("title", "apr_cafef00d"),
                ("level", "warning"),
                ("body", "权限询问"),
                ("body", "⏳ 120s 内有效"),
                ("group", "ion-approvals"),
            ]
            if has_all(p1, shape1):
                pass
            else:
                failures.append(f"payload#2 形状不符: {json.dumps(p1, ensure_ascii=False)}")

        # 断言 5（v2）：推送携带可点击审批页 url + 页面/批准/一次性全链
        tokens = list(_PAGES.keys())
        if PAGE_PORT == 0 or not tokens:
            failures.append(f"v2 审批页未生效（port={PAGE_PORT}, tokens={len(tokens)}）")
        else:
            if not all(str(pp.get("url", "")).startswith(f"http://") and "/p/" in str(pp.get("url", ""))
                       for pp in pushes):
                failures.append("推送缺少 /p/<token> 形式的 url 字段")
            tk = tokens[0]
            base = f"http://127.0.0.1:{PAGE_PORT}"
            try:
                with urllib.request.urlopen(f"{base}/p/{tk}", timeout=5) as r:
                    page1 = r.read().decode("utf-8")
                if "批准" not in page1 or "拒绝" not in page1 or "bus_a.txt" not in page1:
                    failures.append("审批页缺按钮或摘要未转义呈现")
                with urllib.request.urlopen(f"{base}/p/{tk}/approve", timeout=8) as r:
                    page2 = r.read().decode("utf-8")
                if "已批准" not in page2 or "apr_deadbeef" not in page2:
                    failures.append(f"批准结果页异常: {page2[:200]}")
                import urllib.error as _ue
                try:
                    urllib.request.urlopen(f"{base}/p/{tk}/approve", timeout=8)
                    failures.append("一次性链接被重复使用（第二次应 404）")
                except _ue.HTTPError as e:
                    if e.code != 404:
                        failures.append(f"重复使用应 404，实际 {e.code}")
            except Exception as e:
                failures.append(f"审批页链路异常: {type(e).__name__}: {e}")
        return finish(failures, tmp, log_path, dump)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def finish(failures: list, tmp: str, log_path: str, dump: str) -> int:
    if failures:
        print("✘ selftest FAIL:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        for p in (log_path, dump):
            try:
                print(f"  [{p}]", file=sys.stderr)
                with open(p, encoding="utf-8") as fh:
                    for l in fh:
                        print(f"    {l.rstrip()}", file=sys.stderr)
            except OSError:
                pass
        return 1
    print("✔ selftest PASS：2 推送 + 1 冷却去重 + 握手/订阅/payload 形状全对")
    return 0


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(
        prog="approval_push_bridge.py",
        description="ION 统一审批总线 → 手机推送桥（v1）。"
                    "环境变量：ION_APPROVAL_WEBHOOK（必填）/ ION_HOST_SOCKET / ION_APPROVAL_BRIDGE_LOG",
    )
    ap.add_argument("--test-push", action="store_true",
                    help="发一条标题含 TEST 的样例推送（验证手机可达），不连 host")
    ap.add_argument("--with-url", action="store_true",
                    help="配合 --test-push：payload 附 url 字段（v2 可点击链接探针）")
    ap.add_argument("--selftest", action="store_true",
                    help="mock 全链自测（mock socket + mock webhook + 冷却断言），CI 用")
    ap.add_argument("--socket", default=None, help="覆盖 ION_HOST_SOCKET")
    ap.add_argument("--webhook", default=None, help="覆盖 ION_APPROVAL_WEBHOOK")
    ap.add_argument("--log", default=None, help="覆盖 ION_APPROVAL_BRIDGE_LOG")
    ap.add_argument("--no-serve", action="store_true",
                    help="配合 --test-push --with-url：推送后不驻留页面服务（CI 用）")
    ap.add_argument("--http-port", type=int, default=None,
                    help="审批页端口（默认 env ION_APPROVAL_BRIDGE_PORT 或 8793；0=关闭）")
    args = ap.parse_args()

    webhook = args.webhook or os.environ.get("ION_APPROVAL_WEBHOOK", "")
    log_path = args.log or os.environ.get("ION_APPROVAL_BRIDGE_LOG", DEFAULT_LOG)

    if args.selftest:
        return cmd_selftest()

    if not webhook:
        print("✘ 缺少 ION_APPROVAL_WEBHOOK 环境变量（推送网关 URL）。\n"
              "  用法：export ION_APPROVAL_WEBHOOK='https://your-gateway/push' && "
              "python3 scripts/approval_push_bridge.py\n"
              "  人工验证：python3 scripts/approval_push_bridge.py --test-push",
              file=sys.stderr)
        return 2

    if args.test_push:
        return cmd_test_push(webhook, args.with_url, sock_path=os.environ.get(
            "ION_HOST_SOCKET", "").strip() or DEFAULT_SOCKET,
            serve_minutes=0.0 if args.no_serve else 10.0)

    sock_path = args.socket or os.environ.get("ION_HOST_SOCKET", "").strip() or DEFAULT_SOCKET
    http_port = args.http_port
    if http_port is None:
        http_port = int(os.environ.get("ION_APPROVAL_BRIDGE_PORT", "8793") or 0)
    print(f"approval bridge: socket={sock_path} webhook={webhook} log={log_path} page_port={http_port}")
    try:
        return run_bridge(sock_path, webhook, log_path, http_port=http_port)
    except KeyboardInterrupt:
        print("bye")
        return 0


if __name__ == "__main__":
    sys.exit(main())
