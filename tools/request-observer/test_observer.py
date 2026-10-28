import asyncio
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from observer import RequestObserver, STATE_KEY, classify, safe_headers, selected


class Headers(dict):
    def get(self, name, default=None):
        return next((value for key, value in self.items() if key.lower() == name.lower()), default)


def flow_for(payload=None, *, key="dedicated-test-key", path="/v1/messages", body=None):
    if body is None:
        body = json.dumps(payload or {"model": "test", "messages": []}).encode()
    return SimpleNamespace(
        metadata={},
        request=SimpleNamespace(
            path=path, method="POST",
            headers=Headers({"Authorization": "Bearer " + key,
                             "Cookie": "private", "Content-Type": "application/json"}),
            get_content=lambda strict=False: body,
        ),
        response=SimpleNamespace(
            status_code=200,
            headers=Headers({"Content-Type": "text/event-stream", "Set-Cookie": "private"}),
            stream=False,
        ),
    )


class ClassificationTests(unittest.TestCase):
    def test_key_filter_is_explicit_and_does_not_require_a_prefix(self):
        self.assertFalse(selected(Headers({"x-api-key": ""}), ""))
        self.assertFalse(selected(Headers({"x-api-key": "different"}), "test"))
        self.assertTrue(selected(Headers({"X-API-Key": "test"}), "test"))
        self.assertTrue(selected(Headers({"Authorization": "bEaReR test"}), "test"))

    def test_header_allowlist_excludes_credentials_and_cookies(self):
        result = safe_headers(Headers({
            "Authorization": "secret", "x-api-key": "secret", "Cookie": "secret",
            "Set-Cookie": "secret", "Custom-Secret": "secret", "Content-Type": "text/plain",
        }))
        self.assertEqual(result, {"content-type": "text/plain"})

    def test_captures_features_that_rust_request_types_currently_discard(self):
        payload = {
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
            "tool_choice": {"type": "tool", "name": "lookup"},
            "thinking": {"type": "enabled"},
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            "messages": [{"role": "user", "content": [
                {"type": "document", "source": {"type": "url", "url": "https://example.test/a.pdf"}},
                {"type": "image", "source": {"type": "base64", "data": "abc"}},
                {"type": "text", "text": "read these", "cache_control": {"type": "ephemeral"}},
            ]}],
        }
        before = json.dumps(payload)
        names = {item["name"] for item in classify(payload, "/v1/messages")}
        self.assertTrue({"structured_output", "tool_choice", "thinking", "web_search",
                         "document", "image", "cache_control"} <= names)
        self.assertEqual(json.dumps(payload), before)

    def test_probe_wording_is_only_a_heuristic(self):
        labels = classify({"messages": [{"content": "凭直觉选择几个随机整数"}]}, "/v1/messages")
        candidate = next(item for item in labels if item["name"] == "number_fingerprint_candidate")
        self.assertEqual(candidate["certainty"], "heuristic")
        self.assertNotIn("cctest", json.dumps(labels).lower())

    def test_malformed_content_type_does_not_break_classification(self):
        classify({"messages": [{"content": [{"type": {"invalid": True}}]}]}, "/v1/messages")


class CaptureTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.directory = Path(self.temp.name) / "captures"
        self.observer = RequestObserver(key="dedicated-test-key", directory=self.directory,
                                        response_limit=8)

    async def asyncTearDown(self):
        self.temp.cleanup()

    async def test_realistic_lifecycle_preserves_request_and_stream_chunks(self):
        payload = {"model": "test", "stream": True, "unknown_to_rust": {"keep": True},
                   "messages": [{"role": "user", "content": "你好"}]}
        flow = flow_for(payload, path="/v1/messages?api_key=not-recorded")
        original = flow.request.get_content()
        await self.observer.request(flow)
        folder = flow.metadata[STATE_KEY].folder
        self.assertEqual((folder / "request.body").read_bytes(), original)
        self.assertEqual(json.loads((folder / "request.json").read_text(encoding="utf-8")), payload)
        self.observer.responseheaders(flow)
        chunks = [b"data: ", b"hello\n\n", b""]
        for chunk in chunks:
            # The same chunk is immediately available before the response hook.
            self.assertIs(flow.response.stream(chunk), chunk)
        self.assertFalse((folder / "response.meta.json").exists())
        await self.observer.response(flow)
        self.assertEqual((folder / "response.body").read_bytes(), b"data: he")
        meta = json.loads((folder / "response.meta.json").read_text())
        self.assertEqual(meta["body_bytes"], len(b"data: hello\n\n"))
        self.assertTrue(meta["truncated"])
        self.assertIsNotNone(meta["first_body_byte_ms"])
        request_meta = (folder / "request.meta.json").read_text()
        self.assertNotIn("dedicated-test-key", request_meta)
        self.assertNotIn("not-recorded", request_meta)
        self.assertFalse(json.loads(request_meta)["origin_verified"])
        self.assertNotIn(STATE_KEY, flow.metadata)

    async def test_other_keys_and_admin_paths_are_not_captured(self):
        for flow in (flow_for(key="ordinary-client"), flow_for(path="/api/admin/credentials")):
            await self.observer.request(flow)
            self.assertNotIn(STATE_KEY, flow.metadata)
        self.assertFalse(self.directory.exists())
        flow = flow_for(key="ordinary-client")
        self.observer.responseheaders(flow)
        self.assertIs(flow.response.stream, True)

    async def test_parallel_requests_do_not_mix_captures(self):
        first = flow_for({"model": "first", "messages": []})
        second = flow_for({"model": "second", "messages": []})
        await asyncio.gather(self.observer.request(first), self.observer.request(second))
        first_folder = first.metadata[STATE_KEY].folder
        second_folder = second.metadata[STATE_KEY].folder
        self.assertNotEqual(first_folder, second_folder)
        self.observer.responseheaders(first)
        self.observer.responseheaders(second)
        first.response.stream(b"first")
        second.response.stream(b"second")
        await asyncio.gather(self.observer.response(first), self.observer.response(second))
        self.assertEqual((first_folder / "response.body").read_bytes(), b"first")
        self.assertEqual((second_folder / "response.body").read_bytes(), b"second")

    async def test_truncated_requests_are_preserved_as_prefix_without_fake_json(self):
        self.observer.request_limit = 5
        flow = flow_for(body=b'{"model":"long"}')
        await self.observer.request(flow)
        folder = flow.metadata[STATE_KEY].folder
        self.assertEqual((folder / "request.body").read_bytes(), b'{"mod')
        self.assertFalse((folder / "request.json").exists())
        meta = json.loads((folder / "request.meta.json").read_text())
        self.assertTrue(meta["truncated"])
        self.assertEqual(meta["labels"], [])

    async def test_disconnect_keeps_partial_bytes_and_transport_error(self):
        flow = flow_for()
        await self.observer.request(flow)
        folder = flow.metadata[STATE_KEY].folder
        self.observer.responseheaders(flow)
        self.assertEqual(flow.response.stream(b"partial"), b"partial")
        await self.observer.error(flow)
        meta = json.loads((folder / "response.meta.json").read_text())
        self.assertTrue(meta["transport_error"])
        self.assertEqual((folder / "response.body").read_bytes(), b"partial")

    async def test_capture_disk_failure_does_not_reject_the_request(self):
        flow = flow_for()
        with patch.object(self.observer, "save_request", side_effect=OSError("disk unavailable")):
            await self.observer.request(flow)
        self.assertNotIn(STATE_KEY, flow.metadata)
        self.observer.responseheaders(flow)
        self.assertIs(flow.response.stream, True)

    async def test_zero_body_capture_still_forwards_all_bytes(self):
        self.observer.response_limit = 0
        flow = flow_for()
        await self.observer.request(flow)
        folder = flow.metadata[STATE_KEY].folder
        self.observer.responseheaders(flow)
        self.assertEqual(flow.response.stream(b"full response"), b"full response")
        await self.observer.response(flow)
        self.assertEqual((folder / "response.body").read_bytes(), b"")
        self.assertEqual(json.loads((folder / "response.meta.json").read_text())["body_bytes"], 13)


if __name__ == "__main__":
    unittest.main()
