import asyncio
import logging

import httpx
import pytest
from fastapi import HTTPException
from starlette.requests import Request
from uvicorn.middleware.proxy_headers import ProxyHeadersMiddleware

from app.config import config
from app.llm import GeminiClient, LLMError
from app.logging_safe import SafeLogFilter
from app.security import RateLimiter, client_ip


@pytest.mark.parametrize("streaming", [False, True])
def test_key_in_header_and_error_body_private(monkeypatch, caplog, streaming):
    monkeypatch.setattr(config, "gemini_api_key", "test-secret-do-not-log")
    seen = []

    def respond(request):
        seen.append(request)
        return httpx.Response(400, text="test-secret-do-not-log private-question")

    async def run():
        client = GeminiClient()
        await client.close()
        client._client = httpx.AsyncClient(transport=httpx.MockTransport(respond))
        try:
            with pytest.raises(LLMError) as exc:
                if streaming:
                    async for _ in client.stream([]):
                        pass
                else:
                    await client.generate([])
            assert "private-question" not in str(exc.value)
            assert config.gemini_api_key not in str(exc.value)
        finally:
            await client.close()

    with caplog.at_level(logging.INFO, logger="httpx"):
        asyncio.run(run())
    assert seen[0].headers["x-goog-api-key"] == config.gemini_api_key
    assert "key=" not in str(seen[0].url)
    assert config.gemini_api_key not in caplog.text


def test_log_filter_redacts_and_drops_wire_logs(monkeypatch):
    monkeypatch.setattr(config, "gemini_api_key", "test-secret")
    record = logging.LogRecord("campus-policy", 20, "", 1, "value=%s url=?key=unknown", ("test-secret",), None)
    assert SafeLogFilter().filter(record)
    assert "test-secret" not in record.getMessage()
    assert "unknown" not in record.getMessage()
    wire = logging.LogRecord("httpcore.http11", 10, "", 1, "private headers", (), None)
    assert not SafeLogFilter().filter(wire)


def test_spoofed_forwarded_header_does_not_reset_bucket():
    limiter = RateLimiter()
    for i in range(11):
        request = Request({"type": "http", "client": ("198.51.100.10", 1234),
            "headers": [(b"x-forwarded-for", f"203.0.113.{i}".encode())]})
        if i < 10:
            limiter.hit(client_ip(request), 0, 10)
        else:
            with pytest.raises(HTTPException) as exc:
                limiter.hit(client_ip(request), 0, 10)
            assert exc.value.status_code == 429


def test_proxy_trust_boundary():
    received = []

    async def app(scope, receive, send):
        received.append(client_ip(Request(scope)))

    middleware = ProxyHeadersMiddleware(app, trusted_hosts=["127.0.0.1"])
    async def run():
        for peer in ("198.51.100.10", "127.0.0.1"):
            await middleware({"type": "http", "client": (peer, 1234),
                "headers": [(b"x-forwarded-for", b"203.0.113.9")]}, None, None)
    asyncio.run(run())
    assert received == ["198.51.100.10", "203.0.113.9"]


def test_threaded_login_bucket_is_atomic():
    from concurrent.futures import ThreadPoolExecutor
    limiter = RateLimiter()
    def attempt(_):
        try:
            limiter.hit("same-ip", 0, 10)
            return True
        except HTTPException:
            return False
    with ThreadPoolExecutor(max_workers=20) as pool:
        assert sum(pool.map(attempt, range(100))) == 10


def test_stream_preserves_signed_parts_without_exposing_thoughts():
    import json
    original = [{"text": "private-thought", "thought": True}, {"text": "answer"},
        {"functionCall": {"name": "policy_search", "args": {}, "id": "call-1"}, "thoughtSignature": "signature-a"},
        {"text": "", "thoughtSignature": "signature-b"}]
    payload = "".join("data: " + json.dumps({"candidates": [{"content": {"parts": [p]}}]}) + "\n\n" for p in original)
    async def run():
        client = GeminiClient()
        await client.close()
        client._client = httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200, text=payload)))
        try:
            events = [e async for e in client.stream([])]
            assert events[-1]["parts"] == original
            assert "".join(e["text"] for e in events if e["type"] == "text") == "answer"
        finally:
            await client.close()
    asyncio.run(run())


def test_compatibility_checks_use_headers_and_hide_error_bodies(capsys):
    import importlib.util
    from pathlib import Path
    spec = importlib.util.spec_from_file_location("verify_compat", Path(__file__).parents[1] / "scripts" / "verify_compat.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    seen = []
    def response(request):
        seen.append(request)
        return httpx.Response(400, text="private-upstream-secret")
    async def run():
        async with httpx.AsyncClient(transport=httpx.MockTransport(response)) as client:
            await module.gemini_basic(client)
            await module.gemini_function_call(client)
            await module.gemini_stream(client)
            await module.embedding_dims(client)
    asyncio.run(run())
    assert "private-upstream-secret" not in capsys.readouterr().out
    for request in seen[:3]:
        assert "key" not in request.url.params
        assert request.headers["x-goog-api-key"] == config.gemini_api_key
