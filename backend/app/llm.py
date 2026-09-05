"""LLM 客户端：Gemini 原生 /v1beta 反代 + 硅基流动 embedding（查询侧）。

兼容性验证结论（docs/兼容性验证.md）：
- functionResponse 必须回传与 functionCall 一致的 id；
- 流式用 :streamGenerateContent?alt=sse；
- 失败一律显式报错，绝不静默退化为无依据聊天。
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from typing import Any

import httpx
import numpy as np

from .config import config


class LLMError(Exception):
    """面向上层的模型调用失败（消息可直接展示给用户/管理员）。"""


class GeminiClient:
    def __init__(self):
        self._client = httpx.AsyncClient(timeout=httpx.Timeout(120, connect=15))

    async def close(self) -> None:
        await self._client.aclose()

    def _url(self, method: str) -> str:
        return f"{config.gemini_base_url}/models/{config.gemini_model}:{method}"

    async def generate(
        self,
        contents: list[dict],
        system: str | None = None,
        tools: list[dict] | None = None,
        temperature: float = 0.2,
    ) -> list[dict]:
        """非流式调用，返回 parts 列表。"""
        payload: dict[str, Any] = {
            "contents": contents,
            "generationConfig": {"temperature": temperature},
        }
        if system:
            payload["systemInstruction"] = {"parts": [{"text": system}]}
        if tools:
            payload["tools"] = tools
        try:
            r = await self._client.post(self._url("generateContent"), params={"key": config.gemini_api_key}, json=payload)
        except httpx.HTTPError as exc:
            raise LLMError(f"模型服务不可达：{type(exc).__name__}") from exc
        if r.status_code != 200:
            raise LLMError(f"模型调用失败 HTTP {r.status_code}：{r.text[:200]}")
        try:
            return r.json()["candidates"][0]["content"]["parts"]
        except (KeyError, IndexError) as exc:
            raise LLMError("模型响应结构异常") from exc

    async def stream(
        self,
        contents: list[dict],
        system: str | None = None,
        tools: list[dict] | None = None,
        temperature: float = 0.2,
    ) -> AsyncIterator[dict]:
        """流式调用，逐事件 yield：{"type":"text","text":…} | {"type":"parts","parts":[…]}（结束时的完整 parts）。

        累积规则：文本按 part 索引拼接；functionCall 原样独立成 part。
        绝不把 text 与 functionCall 合并进同一 part（oneof 约束，合并会被反代 400 拒绝）。
        """
        payload: dict[str, Any] = {
            "contents": contents,
            "generationConfig": {"temperature": temperature},
        }
        if system:
            payload["systemInstruction"] = {"parts": [{"text": system}]}
        if tools:
            payload["tools"] = tools
        texts: dict[int, list[str]] = {}
        calls: list[dict] = []
        try:
            async with self._client.stream(
                "POST",
                self._url("streamGenerateContent"),
                params={"key": config.gemini_api_key, "alt": "sse"},
                json=payload,
            ) as r:
                if r.status_code != 200:
                    body = (await r.aread()).decode("utf-8", "replace")
                    raise LLMError(f"模型调用失败 HTTP {r.status_code}：{body[:200]}")
                async for line in r.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    data = line[5:].strip()
                    if not data:
                        continue
                    try:
                        obj = json.loads(data)
                    except json.JSONDecodeError:
                        continue
                    try:
                        cparts = obj["candidates"][0]["content"]["parts"]
                    except (KeyError, IndexError):
                        continue
                    for i, p in enumerate(cparts):
                        if "text" in p:
                            texts.setdefault(i, []).append(p["text"])
                            yield {"type": "text", "text": p["text"]}
                        elif "functionCall" in p:
                            calls.append(p["functionCall"])
        except httpx.HTTPError as exc:
            raise LLMError(f"模型流式连接中断：{type(exc).__name__}") from exc
        parts: list[dict] = []
        for i in sorted(texts):
            joined = "".join(texts[i])
            if joined:
                parts.append({"text": joined})
        parts.extend({"functionCall": c} for c in calls)
        yield {"type": "parts", "parts": parts}


async def embed_query(text: str, dims: int | None = None) -> np.ndarray:
    """查询侧向量化：与资料包同模型、同维度。"""
    payload: dict[str, Any] = {"model": config.siliconflow_model, "input": [text]}
    if dims is None:
        dims = config.embed_dims
    payload["dimensions"] = dims
    async with httpx.AsyncClient(timeout=60) as client:
        try:
            r = await client.post(
                "https://api.siliconflow.cn/v1/embeddings",
                headers={"Authorization": f"Bearer {config.siliconflow_api_key}"},
                json=payload,
            )
        except httpx.HTTPError as exc:
            raise LLMError(f"向量化服务不可达：{type(exc).__name__}") from exc
    if r.status_code != 200:
        raise LLMError(f"向量化失败 HTTP {r.status_code}：{r.text[:200]}")
    vec = np.asarray(r.json()["data"][0]["embedding"], dtype=np.float32)
    if vec.shape[0] != dims:
        raise LLMError(f"向量化返回维度 {vec.shape[0]} 与配置 {dims} 不一致")
    n = float(np.linalg.norm(vec))
    if n <= 0:
        raise LLMError("查询向量为零向量")
    return vec / n
