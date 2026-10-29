#!/usr/bin/env python3
"""Transparent, bounded Anthropic HTTP traffic observer. Python standard library only."""

from __future__ import annotations

import base64
import contextlib
import datetime as dt
import hashlib
import hmac
import http.client
import ipaddress
import json
import os
import pathlib
import re
import secrets
import signal
import socket
import sqlite3
import ssl
import threading
import time
import urllib.parse
import uuid
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


CAPTURE_LIMIT = 2 * 1024 * 1024
MAX_REQUEST_BYTES = 50 * 1024 * 1024
MAX_ADMIN_BYTES = 64 * 1024
MAX_EVENT_BYTES = 1024 * 1024
MAX_EVENTS = 10000
RECORD_LIMIT = 200
RETENTION_HOURS = 24
CONNECT_TIMEOUT = 15
READ_TIMEOUT = 120
TOTAL_TIMEOUT = 600
HOP_HEADERS = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te",
    "trailer", "transfer-encoding", "upgrade", "proxy-connection",
}
SECRET_HEADERS = {
    "authorization", "proxy-authorization", "x-api-key", "api-key", "apikey", "cookie",
    "set-cookie", "x-audit-admin-key", "x-goog-api-key", "x-auth-token",
}
STATIC_FILES = {"/": ("index.html", "text/html; charset=utf-8"),
                "/app.js": ("app.js", "text/javascript; charset=utf-8"),
                "/style.css": ("style.css", "text/css; charset=utf-8")}


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def json_bytes(value) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def safe_headers(headers) -> dict[str, str]:
    result = {}
    for name, value in headers:
        lower = name.lower()
        secret = lower in SECRET_HEADERS or bool(re.search(r"(?:^|[-_])(token|secret|api[-_]?key)$", lower))
        result[name] = "[redacted]" if secret else value
    return result


def connection_tokens(headers) -> set[str]:
    return {token.strip().lower() for key, value in headers if key.lower() == "connection"
            for token in value.split(",") if token.strip()}


def parse_base_url(value: str):
    if not isinstance(value, str) or len(value) > 2048 or any(ord(c) < 33 for c in value):
        raise ValueError("Use a valid HTTP(S) upstream base URL without spaces")
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise ValueError("Upstream URL must use http:// or https://")
    if parsed.username is not None or parsed.password is not None or parsed.query or parsed.fragment:
        raise ValueError("Put the API key in its separate field; URL credentials, query and fragment are unsupported")
    try:
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
    except ValueError as error:
        raise ValueError("Invalid upstream port") from error
    if not 1 <= port <= 65535 or "\\" in parsed.netloc:
        raise ValueError("Invalid upstream address")
    host = parsed.hostname.encode("idna").decode("ascii")
    if "%" in host or not host:
        raise ValueError("Invalid upstream host")
    path = parsed.path.rstrip("/")
    if any(part in {".", ".."} for part in urllib.parse.unquote(path).split("/")):
        raise ValueError("Upstream base path cannot contain dot segments")
    return parsed.scheme, host, port, path


def upstream_target(base_url: str, incoming_path: str):
    scheme, host, port, base_path = parse_base_url(base_url)
    parsed = urllib.parse.urlsplit(incoming_path)
    if parsed.scheme or parsed.netloc or not parsed.path.startswith("/v1/"):
        raise ValueError("Only /v1/ API paths can be relayed")
    decoded = urllib.parse.unquote(parsed.path)
    if "\\" in decoded or any(part in {".", ".."} for part in decoded.split("/")):
        raise ValueError("Invalid API path")
    suffix = parsed.path[3:] if base_path.endswith("/v1") else parsed.path
    target = base_path + suffix
    if parsed.query:
        target += "?" + parsed.query
    return scheme, host, port, target


def resolve_addresses(host: str, port: int, allow_private: bool = False):
    try:
        infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except OSError as error:
        raise ValueError("Cannot resolve upstream host") from error
    addresses = []
    for family, socktype, protocol, _, address in infos:
        ip = ipaddress.ip_address(address[0])
        if not allow_private and (not ip.is_global or ip.is_multicast or ip.is_unspecified):
            raise ValueError("Upstream must resolve exclusively to public IP addresses")
        item = (family, socktype, protocol, address)
        if item not in addresses:
            addresses.append(item)
    if not addresses:
        raise ValueError("No usable upstream address")
    return addresses


class PinnedHTTPConnection(http.client.HTTPConnection):
    def __init__(self, host, port, addresses, tls=False):
        super().__init__(host, port, timeout=CONNECT_TIMEOUT)
        self.addresses = addresses
        self.tls = tls

    def connect(self):
        last_error = None
        for family, socktype, protocol, address in self.addresses:
            raw = socket.socket(family, socktype, protocol)
            raw.settimeout(CONNECT_TIMEOUT)
            try:
                raw.connect(address)
                if self.tls:
                    context = ssl.create_default_context()
                    context.set_alpn_protocols(["http/1.1"])
                    self.sock = context.wrap_socket(raw, server_hostname=self.host)
                else:
                    self.sock = raw
                self.sock.settimeout(READ_TIMEOUT)
                return
            except OSError as error:
                raw.close()
                last_error = error
        raise OSError("Cannot connect to the validated upstream address") from last_error


def open_upstream(config, method, incoming_path, body, request_headers, allow_private=False):
    scheme, host, port, target = upstream_target(config["upstreamBaseUrl"], incoming_path)
    addresses = resolve_addresses(host, port, allow_private)
    connection = PinnedHTTPConnection(host, port, addresses, tls=scheme == "https")
    headers = list(request_headers)
    excluded = HOP_HEADERS | connection_tokens(headers) | SECRET_HEADERS | {"host", "content-length"}
    forwarded = [(name, value) for name, value in headers if name.lower() not in excluded]
    connection.putrequest(method, target, skip_host=True, skip_accept_encoding=True)
    authority = "[" + host + "]" if ":" in host else host
    if port != (443 if scheme == "https" else 80):
        authority += ":" + str(port)
    connection.putheader("Host", authority)
    for name, value in forwarded:
        connection.putheader(name, value)
    if config.get("authMode", "x-api-key") == "bearer":
        connection.putheader("Authorization", "Bearer " + config["upstreamApiKey"])
    else:
        connection.putheader("x-api-key", config["upstreamApiKey"])
    if body or method in {"POST", "PUT", "PATCH"}:
        connection.putheader("Content-Length", str(len(body)))
    connection.endheaders(body if body else None)
    try:
        response = connection.getresponse()
        return connection, response
    except Exception:
        connection.close()
        raise


class ConfigStore:
    def __init__(self, directory, public_base_url=None):
        self.directory = pathlib.Path(directory)
        self.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        with contextlib.suppress(OSError):
            self.directory.chmod(0o700)
        self.path = self.directory / "state.json"
        self.lock = threading.RLock()
        self.public_base_url = public_base_url or os.environ.get("AUDIT_PUBLIC_BASE_URL", "")
        if self.path.exists():
            self.state = json.loads(self.path.read_text(encoding="utf-8"))
        else:
            self.state = {
                "admin_key": os.environ.get("AUDIT_ADMIN_KEY") or secrets.token_urlsafe(32),
                "relay_key": os.environ.get("AUDIT_RELAY_KEY") or "sk-audit-" + secrets.token_urlsafe(28),
                "upstreamBaseUrl": "", "upstreamApiKey": "", "authMode": "x-api-key", "enabled": False,
            }
            self._save()
        if not self.state.get("admin_key") or not self.state.get("relay_key"):
            raise ValueError("Audit state is missing authentication keys")

    def _save(self):
        temporary = self.path.with_suffix(".pending")
        with temporary.open("w", encoding="utf-8") as target:
            json.dump(self.state, target, ensure_ascii=False)
            target.flush()
            os.fsync(target.fileno())
        with contextlib.suppress(OSError):
            temporary.chmod(0o600)
        temporary.replace(self.path)
        with contextlib.suppress(OSError):
            self.path.chmod(0o600)

    def snapshot(self):
        with self.lock:
            return dict(self.state)

    def public(self):
        state = self.snapshot()
        key = state.get("upstreamApiKey", "")
        return {
            "upstreamBaseUrl": state.get("upstreamBaseUrl", ""), "hasUpstreamKey": bool(key),
            "upstreamKeyMasked": ("••••" + key[-4:] if len(key) > 4 else "••••") if key else "",
            "authMode": state.get("authMode", "x-api-key"), "enabled": bool(state.get("enabled")),
            "publicBaseUrl": self.public_base_url, "relayKey": state["relay_key"],
            "captureLimitBytes": CAPTURE_LIMIT, "retentionHours": RETENTION_HOURS, "recordLimit": RECORD_LIMIT,
        }

    def update(self, values):
        with self.lock:
            new = dict(self.state)
            if "upstreamBaseUrl" in values:
                value = values["upstreamBaseUrl"]
                if not isinstance(value, str):
                    raise ValueError("upstreamBaseUrl must be a string")
                value = value.strip().rstrip("/")
                if value:
                    parse_base_url(value)
                new["upstreamBaseUrl"] = value
            if "upstreamApiKey" in values:
                value = values["upstreamApiKey"]
                if not isinstance(value, str) or len(value) > 16384 or "\r" in value or "\n" in value:
                    raise ValueError("Invalid upstream API key")
                if value.strip():
                    new["upstreamApiKey"] = value.strip()
            if "authMode" in values:
                if values["authMode"] not in {"x-api-key", "bearer"}:
                    raise ValueError("authMode must be x-api-key or bearer")
                new["authMode"] = values["authMode"]
            if "enabled" in values:
                if not isinstance(values["enabled"], bool):
                    raise ValueError("enabled must be a boolean")
                new["enabled"] = values["enabled"]
            if "clearUpstreamKey" in values:
                if not isinstance(values["clearUpstreamKey"], bool):
                    raise ValueError("clearUpstreamKey must be a boolean")
                if values["clearUpstreamKey"]:
                    new["upstreamApiKey"] = ""
            if new.get("enabled") and not (new.get("upstreamBaseUrl") and new.get("upstreamApiKey")):
                raise ValueError("Provide an upstream URL and API key before enabling relay")
            self.state = new
            self._save()
        return self.public()


def decode_body(body, headers, truncated=False):
    encoding = next((value.lower() for key, value in headers.items() if key.lower() == "content-encoding"), "")
    if encoding and encoding != "identity":
        if truncated:
            return "[Compressed capture is truncated; use bodyBase64 for captured bytes]"
        try:
            if encoding == "gzip":
                decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
            elif encoding == "deflate":
                decoder = zlib.decompressobj()
            else:
                return "[Unsupported compressed view: " + encoding[:30] + "; raw bytes are preserved in bodyBase64]"
            expanded = decoder.decompress(body, CAPTURE_LIMIT * 4 + 1)
            if len(expanded) > CAPTURE_LIMIT * 4 or decoder.unconsumed_tail:
                return "[Decompressed view exceeds the display limit; raw bytes are preserved in bodyBase64]"
            body = expanded
        except zlib.error:
            return "[Cannot decode compressed view; raw bytes are preserved in bodyBase64]"
    return body.decode("utf-8", "replace")


class RecordStore:
    def __init__(self, directory):
        self.path = pathlib.Path(directory) / "captures.db"
        with self.connect() as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("""CREATE TABLE IF NOT EXISTS records(
                id TEXT PRIMARY KEY, created REAL NOT NULL, summary TEXT NOT NULL,
                request_headers TEXT NOT NULL, request_body BLOB NOT NULL,
                response_headers TEXT NOT NULL DEFAULT '{}', response_body BLOB NOT NULL DEFAULT X'',
                events TEXT NOT NULL DEFAULT '[]', analysis TEXT NOT NULL DEFAULT '{}',
                request_hash TEXT NOT NULL, response_hash TEXT NOT NULL DEFAULT '')""")
            db.execute("CREATE INDEX IF NOT EXISTS records_created ON records(created)")
        with contextlib.suppress(OSError):
            self.path.chmod(0o600)

    @contextlib.contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=15)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def create(self, summary, headers, body):
        with self.connect() as db:
            db.execute("INSERT INTO records(id,created,summary,request_headers,request_body,request_hash) VALUES(?,?,?,?,?,?)",
                       (summary["id"], time.time(), json.dumps(summary), json.dumps(headers),
                        body[:CAPTURE_LIMIT], hashlib.sha256(body).hexdigest()))
            db.execute("DELETE FROM records WHERE created < ?", (time.time() - RETENTION_HOURS * 3600,))
            db.execute("DELETE FROM records WHERE id IN (SELECT id FROM records ORDER BY created DESC LIMIT -1 OFFSET ?)",
                       (RECORD_LIMIT,))

    def update(self, summary, headers, body, events, analysis, response_hash):
        with self.connect() as db:
            db.execute("UPDATE records SET summary=?,response_headers=?,response_body=?,events=?,analysis=?,response_hash=? WHERE id=?",
                       (json.dumps(summary), json.dumps(headers), bytes(body), json.dumps(events), json.dumps(analysis),
                        response_hash, summary["id"]))

    def list(self, limit=50, offset=0):
        with self.connect() as db:
            cutoff = time.time() - RETENTION_HOURS * 3600
            total = db.execute("SELECT count(*) FROM records WHERE created>=?", (cutoff,)).fetchone()[0]
            rows = db.execute("SELECT summary FROM records WHERE created>=? ORDER BY created DESC LIMIT ? OFFSET ?",
                              (cutoff, min(200, max(1, limit)), min(10000, max(0, offset))))
            return {"records": [json.loads(row[0]) for row in rows], "total": total}

    def detail(self, record_id):
        with self.connect() as db:
            row = db.execute("SELECT * FROM records WHERE id=? AND created>=?",
                             (record_id, time.time() - RETENTION_HOURS * 3600)).fetchone()
        if row is None:
            return None
        summary = json.loads(row["summary"])
        detail = dict(summary)
        for side in ["request", "response"]:
            headers = json.loads(row[side + "_headers"])
            body = bytes(row[side + "_body"])
            detail[side] = {"headers": headers, "body": decode_body(body, headers, summary.get(side + "Truncated", False)),
                            "bodyBase64": base64.b64encode(body).decode(), "sha256": row[side + "_hash"]}
        detail["events"] = json.loads(row["events"])
        detail["analysis"] = json.loads(row["analysis"])
        return detail

    def ids(self):
        with self.connect() as db:
            return [row[0] for row in db.execute("SELECT id FROM records WHERE created>=? ORDER BY created DESC",
                                                (time.time() - RETENTION_HOURS * 3600,))]

    def clear(self):
        with self.connect() as db:
            db.execute("DELETE FROM records")
            db.commit()
            db.execute("PRAGMA wal_checkpoint(PASSIVE)")


def request_info(body):
    labels = []
    model = None
    stream = False
    try:
        value = json.loads(body)
        if not isinstance(value, dict):
            return model, stream, labels
        model = value.get("model") if isinstance(value.get("model"), str) else None
        stream = bool(value.get("stream"))
        if stream:
            labels.append("streaming")
        if value.get("thinking"):
            labels.append("thinking")
        if value.get("output_config", {}).get("format") or value.get("response_format"):
            labels.append("structured_output")
        tools = value.get("tools", [])
        if isinstance(tools, list):
            if tools:
                labels.append("tools")
            for tool in tools:
                if not isinstance(tool, dict):
                    continue
                kind = str(tool.get("type", ""))
                if kind.startswith("web_search"):
                    labels.append("web_search")
                if kind.startswith("code_execution"):
                    labels.append("server_code_execution")
    except (ValueError, TypeError, AttributeError):
        pass
    return model, stream, list(dict.fromkeys(labels))


class ResponseObserver:
    """Observes decoded views; never supplies data for the forwarded response."""
    def __init__(self, headers):
        lowered = {key.lower(): value for key, value in headers}
        self.is_sse = "text/event-stream" in lowered.get("content-type", "").lower()
        encoding = lowered.get("content-encoding", "").lower()
        self.decoder = None
        self.disabled = False
        self.buffer = bytearray()
        self.events = []
        self.text = []
        self.text_chars = 0
        self.analysis = {"responseModel": None, "contentTypes": [], "toolTypes": [], "toolIdPrefixes": [],
                         "signatureCount": 0, "signatureLengths": [], "signatureHashes": [], "citationCount": 0,
                         "textDeltaCount": 0, "identityPlatform": None, "notes": []}
        if encoding == "gzip":
            self.decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
        elif encoding == "deflate":
            self.decoder = zlib.decompressobj()
        elif encoding and encoding != "identity":
            self.disabled = True
            self.analysis["notes"].append("SSE inspection unavailable for content encoding " + encoding[:30])

    def add_unique(self, field, value):
        if isinstance(value, str) and value not in self.analysis[field] and len(self.analysis[field]) < 100:
            self.analysis[field].append(value[:200])

    def signature(self, value):
        if not isinstance(value, str) or not value:
            return
        self.analysis["signatureCount"] += 1
        if len(self.analysis["signatureLengths"]) < 1000:
            self.analysis["signatureLengths"].append(len(value))
            self.analysis["signatureHashes"].append(hashlib.sha256(value.encode()).hexdigest())

    def append_text(self, value):
        if isinstance(value, str) and self.text_chars < CAPTURE_LIMIT * 4:
            part = value[:max(0, CAPTURE_LIMIT * 4 - self.text_chars)]
            self.text.append(part)
            self.text_chars += len(part)

    def block(self, value):
        if not isinstance(value, dict):
            return
        kind = value.get("type")
        self.add_unique("contentTypes", kind)
        self.signature(value.get("signature"))
        if kind in {"tool_use", "server_tool_use"}:
            self.add_unique("toolTypes", value.get("name"))
            tool_id = value.get("id", "")
            if not isinstance(tool_id, str):
                tool_id = ""
            prefix = next((p for p in ["toolu_bdrk_01", "toolu_", "srvtoolu_", "tooluse_"] if tool_id.startswith(p)), None)
            self.add_unique("toolIdPrefixes", prefix or "other")
        if isinstance(value.get("citations"), list):
            self.analysis["citationCount"] += len(value["citations"])
        if kind == "text" and isinstance(value.get("text"), str):
            self.append_text(value["text"])

    def event(self, value, event_name, milliseconds, size):
        if not isinstance(value, dict):
            return
        kind = value.get("type") or event_name
        block = value.get("content_block", {})
        delta = value.get("delta", {})
        if not isinstance(block, dict):
            block = {}
        if not isinstance(delta, dict):
            delta = {}
        if len(self.events) < MAX_EVENTS:
            self.events.append({"seq": len(self.events) + 1, "ms": round(milliseconds, 3), "type": kind,
                                "blockType": block.get("type"), "deltaType": delta.get("type"),
                                "index": value.get("index"), "bytes": size})
        elif "Event metadata limit reached" not in self.analysis["notes"]:
            self.analysis["notes"].append("Event metadata limit reached")
        if kind == "message_start":
            message = value.get("message")
            if isinstance(message, dict):
                self.analysis["responseModel"] = message.get("model")
        elif kind == "content_block_start":
            self.block(block)
        elif kind == "content_block_delta":
            delta_type = delta.get("type")
            if delta_type == "signature_delta":
                self.signature(delta.get("signature"))
            elif delta_type == "citations_delta":
                self.analysis["citationCount"] += 1
            elif delta_type == "text_delta":
                self.analysis["textDeltaCount"] += 1
                self.append_text(delta.get("text"))

    def feed(self, raw, milliseconds):
        if not self.is_sse or self.disabled:
            return
        try:
            if self.decoder:
                raw = self.decoder.decompress(raw, MAX_EVENT_BYTES * 2)
                if self.decoder.unconsumed_tail:
                    self.disabled = True
                    self.analysis["notes"].append("Compressed inspection expansion limit reached")
            self.buffer.extend(raw)
            while True:
                match = re.search(rb"\r?\n\r?\n", self.buffer)
                if not match:
                    break
                frame = bytes(self.buffer[:match.start()])
                del self.buffer[:match.end()]
                data = []
                event_name = None
                for line in frame.splitlines():
                    if line.startswith(b"data:"):
                        data.append(line[5:].lstrip(b" "))
                    elif line.startswith(b"event:"):
                        event_name = line[6:].strip().decode("utf-8", "replace")
                payload = b"\n".join(data)
                if payload and payload != b"[DONE]":
                    try:
                        self.event(json.loads(payload), event_name, milliseconds, len(frame))
                    except (ValueError, TypeError):
                        if len(self.events) < MAX_EVENTS:
                            self.events.append({"seq": len(self.events) + 1, "ms": round(milliseconds, 3),
                                                "type": event_name or "unparsed", "bytes": len(frame)})
                        elif "Event metadata limit reached" not in self.analysis["notes"]:
                            self.analysis["notes"].append("Event metadata limit reached")
            if len(self.buffer) > MAX_EVENT_BYTES:
                self.disabled = True
                self.buffer.clear()
                self.analysis["notes"].append("SSE event inspection limit reached")
        except Exception:
            self.disabled = True
            self.analysis["notes"].append("SSE inspection stopped; forwarded payload is unchanged")

    def finish(self, captured, headers, truncated):
        if not self.is_sse and not truncated:
            try:
                value = json.loads(decode_body(captured, headers))
                if isinstance(value, dict):
                    self.analysis["responseModel"] = value.get("model")
                    content = value.get("content")
                    if isinstance(content, list):
                        for block in content:
                            self.block(block)
            except (ValueError, TypeError, AttributeError):
                pass
        try:
            value = json.loads("".join(self.text))
            if isinstance(value, dict):
                platform = value.get("identity_platform")
                if isinstance(platform, str):
                    self.analysis["identityPlatform"] = platform[:200]
        except ValueError:
            pass
        return self.analysis


class AuditApp:
    def __init__(self, data_dir, public_base_url=None, allow_private=None):
        self.config = ConfigStore(data_dir, public_base_url)
        self.records = RecordStore(data_dir)
        self.allow_private = (os.environ.get("AUDIT_ALLOW_PRIVATE_UPSTREAM") == "1") if allow_private is None else allow_private
        self.capacity = threading.BoundedSemaphore(16)


class AuditHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False
    allow_reuse_address = True

    def __init__(self, address, app):
        self.app = app
        super().__init__(address, AuditHandler)

    def handle_error(self, request, client_address):
        # Base handler would print a traceback containing untrusted request data.
        print("audit: request handler failed", flush=True)


class AuditHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "upstream-audit"

    @property
    def app(self):
        return self.server.app

    def log_message(self, format, *args):
        pass

    def send_json(self, status, value):
        body = json_bytes(value)
        self.send_response_only(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def fail(self, status, message):
        self.send_json(status, {"error": {"type": "audit_error", "message": message}})

    def authenticated(self, kind):
        values = self.headers.get_all("x-audit-admin-key" if kind == "admin" else "x-api-key", [])
        if kind == "relay" and not values:
            authorization = self.headers.get("Authorization", "")
            if authorization.lower().startswith("bearer "):
                values = [authorization[7:]]
        expected = self.app.config.snapshot()["admin_key" if kind == "admin" else "relay_key"]
        return len(values) == 1 and hmac.compare_digest(values[0].encode(), expected.encode())

    def read_body(self, limit):
        transfers = self.headers.get_all("Transfer-Encoding", [])
        lengths = self.headers.get_all("Content-Length", [])
        if transfers and lengths:
            raise ValueError("Conflicting request body framing")
        if transfers:
            if len(transfers) != 1 or transfers[0].lower().strip() != "chunked":
                raise ValueError("Unsupported request transfer encoding")
            body = bytearray()
            while True:
                line = self.rfile.readline(8193)
                if len(line) > 8192 or not line.endswith(b"\n"):
                    raise ValueError("Invalid request chunk header")
                size = int(line.split(b";", 1)[0].strip(), 16)
                if size < 0 or len(body) + size > limit:
                    raise ValueError("Request body exceeds the configured limit")
                if size == 0:
                    while True:
                        trailer = self.rfile.readline(8193)
                        if trailer in {b"\r\n", b"\n"}:
                            return bytes(body)
                        if not trailer or len(trailer) > 8192:
                            raise ValueError("Invalid request trailers")
                part = self.rfile.read(size)
                if len(part) != size or self.rfile.read(2) != b"\r\n":
                    raise ValueError("Incomplete request chunk")
                body.extend(part)
        if len(set(lengths)) > 1:
            raise ValueError("Conflicting content lengths")
        size = int(lengths[0]) if lengths else 0
        if size < 0 or size > limit:
            raise ValueError("Request body exceeds the configured limit")
        body = self.rfile.read(size)
        if len(body) != size:
            raise ValueError("Incomplete request body")
        return body

    def read_json(self):
        if self.headers.get_content_type() != "application/json":
            raise ValueError("Use Content-Type: application/json")
        value = json.loads(self.read_body(MAX_ADMIN_BYTES))
        if not isinstance(value, dict):
            raise ValueError("Expected a JSON object")
        return value

    def static(self, path):
        filename, content_type = STATIC_FILES[path]
        target = pathlib.Path(__file__).parent / "static" / filename
        if not target.is_file():
            self.fail(404, "Dashboard asset is unavailable")
            return
        body = target.read_bytes()
        self.send_response_only(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def dispatch(self):
        self.connection.settimeout(READ_TIMEOUT)
        path = urllib.parse.urlsplit(self.path).path
        if self.command in {"GET", "HEAD"} and path in STATIC_FILES:
            return self.static(path)
        if path in {"/health", "/healthz"} and self.command == "GET":
            return self.send_json(200, {"ok": True})
        if path.startswith("/api/"):
            if not self.authenticated("admin"):
                self.close_connection = True
                return self.fail(401, "A valid dashboard key is required")
            try:
                return self.admin(path)
            except (ValueError, TypeError, json.JSONDecodeError) as error:
                self.close_connection = True
                return self.fail(400, str(error)[:250])
        if path.startswith("/v1/"):
            if not self.authenticated("relay"):
                self.close_connection = True
                return self.fail(401, "A valid relay key is required")
            return self.relay()
        self.close_connection = True
        self.fail(404, "Not found")

    def admin(self, path):
        if path == "/api/config":
            if self.command == "GET":
                return self.send_json(200, self.app.config.public())
            if self.command == "PUT":
                return self.send_json(200, self.app.config.update(self.read_json()))
        if path == "/api/records" and self.command == "GET":
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
            return self.send_json(200, self.app.records.list(int(query.get("limit", [50])[0]), int(query.get("offset", [0])[0])))
        if path.startswith("/api/records/") and self.command == "GET":
            record_id = path[len("/api/records/"):]
            if not re.fullmatch(r"[0-9a-f-]{36}", record_id):
                return self.fail(404, "Record not found")
            detail = self.app.records.detail(record_id)
            return self.send_json(200, detail) if detail else self.fail(404, "Record not found")
        if path == "/api/check" and self.command == "POST":
            self.read_json()
            config = self.app.config.snapshot()
            if not config.get("upstreamBaseUrl") or not config.get("upstreamApiKey"):
                return self.send_json(200, {"ok": False, "status": 0, "modelCount": 0, "error": "Save an upstream URL and key first"})
            connection = None
            try:
                check_headers = [("Accept", "application/json"), ("anthropic-version", "2023-06-01"),
                                 ("User-Agent", "upstream-audit/1.0")]
                connection, response = open_upstream(config, "GET", "/v1/models", b"", check_headers, self.app.allow_private)
                body = response.read(CAPTURE_LIMIT + 1)
                count = 0
                if response.status == 200 and len(body) <= CAPTURE_LIMIT:
                    with contextlib.suppress(ValueError, AttributeError):
                        count = len(json.loads(body).get("data", []))
                return self.send_json(200, {"ok": 200 <= response.status < 300, "status": response.status,
                                            "modelCount": count, "error": "" if 200 <= response.status < 300 else "Upstream returned a non-success status"})
            except Exception as error:
                return self.send_json(200, {"ok": False, "status": 0, "modelCount": 0,
                                            "error": str(error)[:250] if isinstance(error, ValueError) else type(error).__name__})
            finally:
                if connection:
                    connection.close()
        if path == "/api/clear" and self.command == "POST":
            if self.read_json().get("confirm") is not True:
                raise ValueError("Explicit confirmation is required")
            self.app.records.clear()
            return self.send_json(200, {"ok": True})
        if path == "/api/export" and self.command == "GET":
            self.send_response_only(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Disposition", 'attachment; filename="upstream-audit-' + dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + '.json"')
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            config = self.app.config.snapshot()
            metadata = {"exportedAt": utc_now(), "config": {key: config.get(key) for key in ["upstreamBaseUrl", "authMode", "enabled"]}}
            first = json_bytes(metadata)[:-1] + b',"records":['
            self.write_chunk(first)
            written = False
            for record_id in self.app.records.ids():
                detail = self.app.records.detail(record_id)
                if detail:
                    self.write_chunk((b"," if written else b"") + json_bytes(detail))
                    written = True
            self.write_chunk(b"]}")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return
        self.fail(404, "Not found")

    def write_chunk(self, data):
        if data:
            self.wfile.write(("%x\r\n" % len(data)).encode("ascii"))
            self.wfile.write(data)
            self.wfile.write(b"\r\n")
            self.wfile.flush()

    def relay(self):
        if not self.app.capacity.acquire(blocking=False):
            self.close_connection = True
            return self.fail(503, "Monitor is at concurrent request capacity")
        try:
            return self._relay()
        finally:
            self.app.capacity.release()

    def _relay(self):
        config = self.app.config.snapshot()
        if not config.get("enabled"):
            self.close_connection = True
            return self.fail(503, "Relay is disabled in the dashboard")
        try:
            body = self.read_body(MAX_REQUEST_BYTES)
        except (ValueError, OSError) as error:
            self.close_connection = True
            return self.fail(400, str(error)[:250])
        model, stream, labels = request_info(body)
        started = time.monotonic()
        # Query values can contain credentials; only the URL path is persisted.
        summary = {"id": str(uuid.uuid4()), "startedAt": utc_now(), "method": self.command,
                   "path": urllib.parse.urlsplit(self.path).path, "status": None, "model": model,
                   "labels": labels, "stream": stream, "durationMs": 0, "firstByteMs": None,
                   "requestBytes": len(body), "responseBytes": 0, "requestTruncated": len(body) > CAPTURE_LIMIT,
                   "responseTruncated": False, "state": "running", "error": None}
        recorded = True
        try:
            self.app.records.create(summary, safe_headers(self.headers.items()), body)
        except sqlite3.Error:
            recorded = False
            print("audit: request capture could not be initialized", flush=True)
        connection = None
        response_headers = {}
        captured = bytearray()
        digest = hashlib.sha256()
        observer = ResponseObserver([])
        sent_headers = False
        last_save = started
        chunked = True
        try:
            connection, response = open_upstream(config, self.command, self.path, body, list(self.headers.items()), self.app.allow_private)
            original_headers = response.getheaders()
            response_headers = safe_headers(original_headers)
            observer = ResponseObserver(original_headers)
            summary["status"] = response.status
            excluded = HOP_HEADERS | connection_tokens(original_headers)
            no_body = self.command == "HEAD" or response.status in {204, 304} or 100 <= response.status < 200
            lengths = [value.strip() for key, value in original_headers if key.lower() == "content-length"]
            if len(set(lengths)) > 1:
                raise ValueError("Upstream returned conflicting content lengths")
            if response.chunked:
                excluded.add("content-length")
            chunked = not no_body and (response.chunked or response.length is None)
            self.send_response_only(response.status, response.reason)
            for key, value in original_headers:
                if key.lower() not in excluded:
                    self.send_header(key, value)
            if chunked:
                self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.flush()
            sent_headers = True
            while not no_body:
                if time.monotonic() - started > TOTAL_TIMEOUT:
                    raise TimeoutError("Upstream request duration limit")
                part = response.read1(64 * 1024)
                if not part:
                    break
                milliseconds = (time.monotonic() - started) * 1000
                if summary["firstByteMs"] is None:
                    summary["firstByteMs"] = round(milliseconds, 3)
                summary["responseBytes"] += len(part)
                digest.update(part)
                captured.extend(part[:max(0, CAPTURE_LIMIT - len(captured))])
                summary["responseTruncated"] = summary["responseBytes"] > CAPTURE_LIMIT
                observer.feed(part, milliseconds)
                if chunked:
                    self.write_chunk(part)
                else:
                    self.wfile.write(part)
                    self.wfile.flush()
                if recorded and time.monotonic() - last_save >= .5:
                    summary["durationMs"] = round((time.monotonic() - started) * 1000, 3)
                    try:
                        self.app.records.update(summary, response_headers, captured, observer.events, observer.analysis, digest.hexdigest())
                    except sqlite3.Error:
                        pass
                    last_save = time.monotonic()
            if not no_body and response.length not in {None, 0}:
                raise http.client.IncompleteRead(b"")
            if chunked:
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            summary["state"] = "complete" if response.status < 400 else "error"
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            summary["state"] = "disconnected"
            summary["error"] = "Client disconnected; upstream connection was closed"
            self.close_connection = True
        except Exception as error:
            summary["state"] = "error"
            summary["error"] = str(error)[:250] if isinstance(error, ValueError) else type(error).__name__
            self.close_connection = True
            if not sent_headers:
                summary["status"] = 502
                with contextlib.suppress(OSError):
                    self.fail(502, "Upstream connection failed (" + summary["error"] + ")")
        finally:
            if connection:
                connection.close()
            summary["durationMs"] = round((time.monotonic() - started) * 1000, 3)
            try:
                analysis = observer.finish(captured, response_headers, summary["responseTruncated"])
            except Exception:
                analysis = observer.analysis
                analysis["notes"].append("Final response inspection failed; forwarded payload was unchanged")
            if recorded:
                try:
                    self.app.records.update(summary, response_headers, captured, observer.events, analysis, digest.hexdigest())
                except sqlite3.Error:
                    print("audit: response capture could not be saved", flush=True)

    do_GET = dispatch
    do_HEAD = dispatch
    do_POST = dispatch
    do_PUT = dispatch
    do_PATCH = dispatch
    do_DELETE = dispatch
    do_OPTIONS = dispatch


def main():
    data_dir = os.environ.get("AUDIT_DATA_DIR", str(pathlib.Path(__file__).parent / "data"))
    host = os.environ.get("AUDIT_HOST", "0.0.0.0")
    port = int(os.environ.get("AUDIT_PORT", "8991"))
    app = AuditApp(data_dir)
    server = AuditHTTPServer((host, port), app)
    def stop(signum, frame):
        threading.Thread(target=server.shutdown, daemon=True).start()
    for sig in [signal.SIGTERM, signal.SIGINT]:
        signal.signal(sig, stop)
    print("upstream-audit listening on " + host + ":" + str(port), flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
