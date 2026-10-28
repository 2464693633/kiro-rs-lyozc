"""Observe a dedicated test key in mitmproxy reverse mode, without rewriting traffic."""

from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import logging
import os
import re
import time
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


LOGGER = logging.getLogger(__name__)
PATHS = {
    "/v1/models", "/v1/messages", "/v1/messages/count_tokens",
    "/v1/chat/completions", "/v1/responses", "/cc/v1/messages",
    "/cc/v1/messages/count_tokens",
}
SAFE_HEADERS = {
    "content-type", "content-encoding", "anthropic-version", "anthropic-beta",
    "user-agent", "request-id", "x-request-id", "retry-after",
    "content-length", "cache-control",
}
STATE_KEY = "kiro_request_observer"


def selected(headers: Any, key: str) -> bool:
    if not key:
        return False
    candidates = [headers.get("x-api-key", "")]
    auth = headers.get("authorization", "")
    scheme, _, value = auth.partition(" ")
    if scheme.lower() == "bearer":
        candidates.append(value.strip())
    return any(hmac.compare_digest(v.encode(), key.encode()) for v in candidates)


def safe_headers(headers: Any) -> dict[str, str]:
    # An allowlist deliberately excludes Authorization, x-api-key and cookies.
    return {str(k).lower(): str(v) for k, v in headers.items()
            if str(k).lower() in SAFE_HEADERS}


def classify(payload: Any, path: str) -> list[dict[str, str]]:
    """Describe observable capabilities; heuristic labels never prove an origin."""
    if not isinstance(payload, dict):
        return []
    labels: dict[str, dict[str, str]] = {}

    def add(name: str, evidence: str, certainty: str = "observed") -> None:
        labels[name] = {"name": name, "certainty": certainty, "evidence": evidence}

    if path.endswith("/count_tokens"):
        add("token_count", "Request targets count_tokens")
    if path.endswith("/models"):
        add("model_listing", "Request targets models")
    if path not in {"/v1/models", "/v1/messages/count_tokens", "/cc/v1/messages/count_tokens"}:
        add("streaming" if payload.get("stream") is True else "non_streaming",
            "Requested response mode")
    thinking = payload.get("thinking")
    if isinstance(thinking, dict) and thinking.get("type") in {"enabled", "adaptive"}:
        add("thinking", "thinking.type=" + thinking["type"])
    output = payload.get("output_config")
    response_format = payload.get("response_format")
    text_format = payload.get("text")
    if ((isinstance(output, dict) and "format" in output)
            or isinstance(response_format, dict)
            or (isinstance(text_format, dict) and "format" in text_format)):
        add("structured_output", "An output format constraint is present")
    if payload.get("tool_choice") is not None:
        add("tool_choice", "Explicit tool_choice is present")
    tools = payload.get("tools")
    if isinstance(tools, list) and tools:
        add("tools", "Nonempty tools declaration")
        for tool in tools:
            if not isinstance(tool, dict):
                continue
            kind = str(tool.get("type", ""))
            if kind.startswith("web_search") or tool.get("name") == "web_search":
                add("web_search", "Search tool declaration")
            if tool.get("strict") is True:
                add("strict_tool", "Tool declares strict=true")

    # JSON comes from a bounded capture. Walk iteratively and cap heuristic text.
    stack = [payload]
    texts: list[str] = []
    remaining = 65536
    while stack:
        item = stack.pop()
        if isinstance(item, dict):
            kind = item.get("type")
            if isinstance(kind, str) and kind in {"image", "image_url", "input_image"}:
                add("image", "Image content block")
            if isinstance(kind, str) and kind in {"document", "input_file", "file"}:
                add("document", "Document/file content block")
            if isinstance(kind, str) and kind in {"thinking", "redacted_thinking"}:
                add("thinking_replay", "Thinking content supplied in request history")
            if item.get("signature") is not None:
                add("signature_replay", "A signature field is supplied in the request")
            if "cache_control" in item:
                add("cache_control", "Cache breakpoint/control declaration")
            for name in ("text", "content", "instructions", "input"):
                value = item.get(name)
                if isinstance(value, str) and remaining > 0:
                    texts.append(value[:remaining])
                    remaining -= min(len(value), remaining)
            stack.extend(v for v in item.values() if isinstance(v, (dict, list)))
        elif isinstance(item, list):
            stack.extend(reversed(item))
    text = "\n".join(texts).lower()
    if re.search(r"(凭直觉|随机|random|intuitiv)", text) and re.search(
            r"(整数|数字|integer|numbers?)", text):
        add("number_fingerprint_candidate", "Number picking/randomness wording",
            "heuristic")
    if re.search(r"(知识截止|训练截止|knowledge cut.?off|training cut.?off)", text):
        add("knowledge_probe_candidate", "Knowledge/training cutoff wording", "heuristic")
    if re.search(r"(你是什么模型|你是哪个模型|which model are you|who developed you)", text):
        add("identity_probe_candidate", "Model identity wording", "heuristic")
    return list(labels.values())


def byte_limit(name: str, default: int) -> int:
    try:
        value = int(os.environ.get(name, str(default)))
        if 0 <= value <= 64 * 1024 * 1024:
            return value
    except ValueError:
        pass
    LOGGER.warning("Invalid %s; using %s", name, default)
    return default


@dataclass
class Capture:
    folder: Path
    started: float
    response_limit: int
    response: bytearray = field(default_factory=bytearray)
    response_bytes: int = 0
    first_byte_ms: float | None = None

    def observe_chunk(self, chunk: bytes) -> bytes:
        if chunk:
            if self.first_byte_ms is None:
                self.first_byte_ms = round((time.monotonic() - self.started) * 1000, 2)
            self.response_bytes += len(chunk)
            available = max(0, self.response_limit - len(self.response))
            self.response.extend(chunk[:available])
        return chunk


class RequestObserver:
    def __init__(self, *, key: str | None = None, directory: Path | None = None,
                 request_limit: int | None = None, response_limit: int | None = None):
        self.key = key if key is not None else os.environ.get("KIRO_OBSERVER_API_KEY", "")
        self.directory = directory if directory is not None else Path(os.environ.get(
            "KIRO_OBSERVER_DIR", str(Path(__file__).parent / "captures")))
        self.request_limit = request_limit if request_limit is not None else byte_limit(
            "KIRO_OBSERVER_REQUEST_BYTES", 8 * 1024 * 1024)
        self.response_limit = response_limit if response_limit is not None else byte_limit(
            "KIRO_OBSERVER_RESPONSE_BYTES", 2 * 1024 * 1024)
        self.label = os.environ.get("KIRO_OBSERVER_LABEL", "manual-test")

    def running(self) -> None:
        if not self.key:
            LOGGER.warning("Request observer disabled: set KIRO_OBSERVER_API_KEY")

    @staticmethod
    def write_json(path: Path, value: Any) -> None:
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
        temporary.replace(path)

    def save_request(self, state: Capture, body: bytes, metadata: dict[str, Any]) -> None:
        state.folder.mkdir(parents=True, exist_ok=True)
        captured = body[:self.request_limit]
        (state.folder / "request.body").write_bytes(captured)
        metadata.update({"body_bytes": len(body), "captured_bytes": len(captured),
                         "truncated": len(captured) < len(body),
                         "body_sha256": hashlib.sha256(body).hexdigest()})
        try:
            payload = json.loads(captured) if len(captured) == len(body) else None
            metadata["labels"] = classify(payload, metadata["path"])
            if isinstance(payload, dict):
                # The original payload, including fields kiro-rs might discard.
                self.write_json(state.folder / "request.json", payload)
                metadata["model"] = payload.get("model")
        except (ValueError, RecursionError, UnicodeError):
            metadata["labels"] = []
            metadata["parse_note"] = "Body is not supported JSON; see request.body"
        self.write_json(state.folder / "request.meta.json", metadata)

    async def request(self, flow: Any) -> None:
        path = flow.request.path.partition("?")[0].rstrip("/")
        if path not in PATHS or not selected(flow.request.headers, self.key):
            return
        now = datetime.now(timezone.utc)
        capture_id = now.strftime("%Y%m%dT%H%M%S.%fZ_") + uuid.uuid4().hex[:12]
        state = Capture(self.directory / capture_id, time.monotonic(), self.response_limit)
        metadata = {"capture_id": capture_id, "time_utc": now.isoformat(),
                    "operator_label": self.label, "origin_verified": False,
                    "method": flow.request.method, "path": path,
                    "headers": safe_headers(flow.request.headers)}
        try:
            body = flow.request.get_content(strict=False) or b""
            await asyncio.to_thread(self.save_request, state, body, metadata)
            flow.metadata[STATE_KEY] = state
            LOGGER.info("Observed %s %s -> %s", flow.request.method, path, capture_id)
        except Exception:
            # Capture failures must not turn a valid API call into a failure.
            LOGGER.error("Could not save request capture; forwarding continues")

    def responseheaders(self, flow: Any) -> None:
        path = flow.request.path.partition("?")[0].rstrip("/")
        if path not in PATHS:
            return
        state = flow.metadata.get(STATE_KEY)
        # Preserve live SSE for both observed and unobserved API traffic.
        flow.response.stream = state.observe_chunk if state else True

    def save_response(self, state: Capture, metadata: dict[str, Any]) -> None:
        metadata.update({"duration_ms": round((time.monotonic() - state.started) * 1000, 2),
                         "first_body_byte_ms": state.first_byte_ms,
                         "body_bytes": state.response_bytes,
                         "captured_bytes": len(state.response),
                         "truncated": len(state.response) < state.response_bytes,
                         "body_encoding": "HTTP content bytes; see Content-Encoding"})
        (state.folder / "response.body").write_bytes(state.response)
        self.write_json(state.folder / "response.meta.json", metadata)

    async def finish(self, flow: Any, failed: bool) -> None:
        state = flow.metadata.pop(STATE_KEY, None)
        if state is None:
            return
        response = flow.response
        metadata = {"status_code": response.status_code if response is not None else None,
                    "headers": safe_headers(response.headers) if response is not None else {},
                    "transport_error": failed}
        try:
            await asyncio.to_thread(self.save_response, state, metadata)
        except Exception:
            LOGGER.error("Could not save response capture; forwarding continues")

    async def response(self, flow: Any) -> None:
        await self.finish(flow, False)

    async def error(self, flow: Any) -> None:
        await self.finish(flow, True)


addons = [RequestObserver()]
