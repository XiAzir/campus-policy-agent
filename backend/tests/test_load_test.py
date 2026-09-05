import asyncio
import json
import time

import httpx

from scripts.load_test import prepare_tokens, run_one
from app.llm import GeminiClient
from app.metrics import TurnMetrics, current_metrics


def test_prepare_tokens_waits_on_login_limit():
    calls = 0
    sleeps = []
    def respond(request):
        nonlocal calls
        calls += 1
        return httpx.Response(429 if calls == 11 else 200, json={"token": f"t{calls}"})
    with httpx.Client(transport=httpx.MockTransport(respond)) as client:
        tokens = prepare_tokens(client, "https://test", "code", 11, sleep=sleeps.append)
    assert len(tokens) == 11 and calls == 12 and sleeps == [31]


class TimedEvents(httpx.SyncByteStream):
    def __iter__(self):
        yield b'data: {"event":"started","request_id":"x"}\n\n'
        time.sleep(0.08)
        yield b'data: {"event":"metrics","retrieval_s":0.02,"model_calls":2,"prompt_tokens":12,"model_usage_reported":true}\n\n'
        yield b'data: {"event":"done","text":"PRIVATE_ANSWER","interrupted":false}\n\n'


def test_queue_clock_is_recorded_before_response_finishes():
    def factory(**kwargs):
        return httpx.Client(transport=httpx.MockTransport(lambda _: httpx.Response(200, stream=TimedEvents())), **kwargs)
    row = run_one("https://test", "secret-token", "private-question", 1, client_factory=factory)
    assert row["queued_s"] < row["total_s"] - 0.05
    assert row["done"] and row["prompt_tokens"] == 12 and row["retrieval_s"] == 0.02
    assert "PRIVATE_ANSWER" not in json.dumps(row) and "secret-token" not in json.dumps(row)


def test_stream_usage_metadata_is_counted_once():
    events = [
        {"candidates": [{"content": {"parts": [{"text": "one"}]}}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 1}},
        {"candidates": [{"content": {"parts": [{"text": "two"}]}}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 2}},
    ]
    async def run():
        metrics = TurnMetrics()
        token = current_metrics.set(metrics)
        client = GeminiClient()
        await client.close()
        client._client = httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200, text="".join("data: " + json.dumps(e) + "\n\n" for e in events))))
        try:
            async for _ in client.stream([]):
                pass
            assert metrics.model_calls == 1 and metrics.prompt_tokens == 10 and metrics.output_tokens == 2
            assert metrics.model_usage_reported
        finally:
            current_metrics.reset(token)
            await client.close()
    asyncio.run(run())
