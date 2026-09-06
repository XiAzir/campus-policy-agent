"""兼容性验证脚本（plan.md 第五节第 1 步）。

逐项验证：
1. Gemini 原生 /v1beta 反代：普通生成、函数调用、流式（SSE）。
2. 硅基流动 embedding：dims 参数、实际返回维度。
3. 中文检索：jieba 分词 + SQLite FTS5 往返、向量余弦召回。
4. 引用：文档哈希 + 行号区间的确定性 ID 生成与校验。

用法：.venv/Scripts/python backend/scripts/verify_compat.py
结果输出到 docs/兼容性验证.md 汇总。日志不打印任何密钥。
"""

from __future__ import annotations

import asyncio
import argparse
import json
import os
import sqlite3
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "backend"))

from dotenv import load_dotenv  # noqa: E402

load_dotenv(REPO / ".env")

GEMINI_BASE = os.environ["GEMINI_BASE_URL"].rstrip("/")
GEMINI_KEY = os.environ["GEMINI_API_KEY"]
GEMINI_MODEL = os.environ["GEMINI_MODEL"]
SF_KEY = os.environ["SILICONFLOW_API_KEY"]
SF_MODEL = os.environ["SILICONFLOW_EMBED_MODEL"]
SF_DIMS = int(os.environ.get("EMBED_DIMS", "1024"))

import httpx  # noqa: E402
import jieba  # noqa: E402
import numpy as np  # noqa: E402
from app.logging_safe import configure_logging  # noqa: E402

configure_logging()

RESULTS: list[dict] = []


def record(step: str, ok: bool, detail: str) -> None:
    RESULTS.append({"step": step, "ok": ok, "detail": detail})
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {step}: {detail}")


async def gemini_basic(client: httpx.AsyncClient) -> None:
    url = f"{GEMINI_BASE}/models/{GEMINI_MODEL}:generateContent"
    payload = {
        "contents": [{"role": "user", "parts": [{"text": "只回复两个字：正常"}]}],
        "generationConfig": {"temperature": 0},
    }
    t0 = time.time()
    r = await client.post(url, headers={"x-goog-api-key": GEMINI_KEY}, json=payload, timeout=60)
    if r.status_code != 200:
        record("Gemini 普通生成", False, f"HTTP {r.status_code}")
        return
    data = r.json()
    try:
        text = data["candidates"][0]["content"]["parts"][0]["text"]
    except (KeyError, IndexError):
        record("Gemini 普通生成", False, "响应结构异常")
        return
    record("Gemini 普通生成", bool(text.strip()), f"收到文本，耗时{time.time()-t0:.1f}s；使用请求头鉴权")


DECLARE_SEARCH_TOOL = {
    "function_declarations": [
        {
            "name": "policy_search",
            "description": "在班级政策资料库中检索与问题相关的政策原文片段。",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": {"type": "STRING", "description": "检索关键词或问句"},
                },
                "required": ["query"],
            },
        }
    ]
}

FAKE_SEARCH_RESULT = {
    "results": [
        {
            "evidence_id": "EV1",
            "citation_id": "a" * 64 + ":12-15",
            "title": "测试管理办法",
            "lines": ["L12: 旷课累计达到十学时的，给予警告处分。"],
        }
    ]
}


async def gemini_function_call(client: httpx.AsyncClient) -> None:
    """两轮工具调用往返：模型发 functionCall → 回填 functionResponse → 模型给文字结论。"""
    url = f"{GEMINI_BASE}/models/{GEMINI_MODEL}:generateContent"
    contents = [
        {
            "role": "user",
            "parts": [{"text": "请查一下旷课十学时会受什么处分，用检索工具查。"}],
        }
    ]
    ok = False
    detail = ""
    for round_no in range(3):
        payload = {
            "contents": contents,
            "tools": [DECLARE_SEARCH_TOOL],
            "generationConfig": {"temperature": 0},
        }
        r = await client.post(url, headers={"x-goog-api-key": GEMINI_KEY}, json=payload, timeout=90)
        if r.status_code != 200:
            record("Gemini 函数调用", False, f"第{round_no+1}轮 HTTP {r.status_code}")
            return
        parts = r.json()["candidates"][0]["content"]["parts"]
        contents.append({"role": "model", "parts": parts})
        calls = [p["functionCall"] for p in parts if "functionCall" in p]
        if not calls:
            texts = "".join(p.get("text", "") for p in parts)
            ok = "警告" in texts
            detail = f"第{round_no+1}轮得到文字结论，符合合成依据={ok}"
            break
        fr_parts = []
        for c in calls:
            # 本反代要求 functionResponse 携带与 functionCall 一致的 id
            resp: dict = {"name": c["name"], "response": FAKE_SEARCH_RESULT}
            if "id" in c:
                resp["id"] = c["id"]
            fr_parts.append({"functionResponse": resp})
        contents.append({"role": "user", "parts": fr_parts})
    record("Gemini 函数调用", ok, detail or "未在 3 轮内得到结论")


async def gemini_stream(client: httpx.AsyncClient) -> None:
    url = f"{GEMINI_BASE}/models/{GEMINI_MODEL}:streamGenerateContent"
    payload = {
        "contents": [{"role": "user", "parts": [{"text": "从1数到5，只给数字"}]}],
        "generationConfig": {"temperature": 0},
    }
    chunks = 0
    text_parts: list[str] = []
    try:
        async with client.stream(
            "POST", url, headers={"x-goog-api-key": GEMINI_KEY}, params={"alt": "sse"}, json=payload, timeout=90
        ) as r:
            if r.status_code != 200:
                record("Gemini 流式 SSE", False, f"HTTP {r.status_code}")
                return
            async for line in r.aiter_lines():
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data:
                    continue
                obj = json.loads(data)
                for p in obj.get("candidates", [{}])[0].get("content", {}).get("parts", []):
                    if "text" in p:
                        text_parts.append(p["text"])
                chunks += 1
        record(
            "Gemini 流式 SSE",
            chunks > 0,
            f"收到 {chunks} 个 SSE 事件，文本字符数={sum(map(len, text_parts))}",
        )
    except Exception as exc:  # noqa: BLE001
        record("Gemini 流式 SSE", False, f"异常 {type(exc).__name__}")


async def embed(client: httpx.AsyncClient, texts: list[str], dims: int | None) -> tuple[bool, list[list[float]] | None, str]:
    payload: dict = {"model": SF_MODEL, "input": texts}
    if dims is not None:
        payload["dimensions"] = dims
    r = await client.post(
        "https://api.siliconflow.cn/v1/embeddings",
        headers={"Authorization": f"Bearer {SF_KEY}"},
        json=payload,
        timeout=120,
    )
    if r.status_code != 200:
        return False, None, f"HTTP {r.status_code}"
    data = r.json()
    vecs = [item["embedding"] for item in data["data"]]
    return True, vecs, f"返回 {len(vecs[0])} 维"


async def embedding_dims(client: httpx.AsyncClient) -> None:
    ok, vecs, detail = await embed(client, ["旷课处分规定"], SF_DIMS)
    if not ok:
        record("Embedding dims=1024", False, detail)
        ok2, vecs2, detail2 = await embed(client, ["旷课处分规定"], None)
        record("Embedding 默认维度", ok2, detail2 if not ok2 else f"{detail2}（dims 参数不被接受，需按此维度重建）")
        return
    record("Embedding dims=1024", vecs is not None and len(vecs[0]) == SF_DIMS, detail)


def fts_jieba() -> None:
    conn = sqlite3.connect(":memory:")
    try:
        conn.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
    except sqlite3.OperationalError as exc:
        record("SQLite FTS5 可用性", False, str(exc))
        return
    docs = [
        "学生旷课累计达到十学时的，给予警告处分；达到二十学时的，给予严重警告处分。",
        "宿舍内严禁使用大功率电器，违者没收并处以警告处分。",
        "共青团员应当按时缴纳团费，连续六个月不缴纳团费的按自行脱团处理。",
    ]
    for d in docs:
        conn.execute("INSERT INTO t(body) VALUES (?)", (" ".join(jieba.cut(d)),))
    # 查询侧与写入侧同一分词；FTS5 用 OR 语义避免整句 AND 零命中，靠 bm25 排序
    tokens = [t.strip() for t in jieba.cut("旷课会有什么处分") if t.strip()]
    q = " OR ".join(f'"{t}"' for t in tokens)
    hits = conn.execute(
        "SELECT body FROM t WHERE t MATCH ? ORDER BY bm25(t)", (q,)
    ).fetchall()
    ok = len(hits) > 0 and "旷课" in hits[0][0]
    record("jieba + FTS5 中文检索", ok, f"查询'旷课会有什么处分'命中 {len(hits)} 条，首条含'旷课'={ok}")


async def vector_recall() -> None:
    corpus = [
        "学生旷课累计达到十学时的，给予警告处分。",
        "宿舍内严禁使用大功率电器。",
        "团员连续六个月不缴纳团费按自行脱团处理。",
    ]
    queries = ["逃课多少学时会被处分", "宿舍能不能用电火锅", "不交团费会怎样"]
    expected = [0, 1, 2]

    async def run() -> tuple[bool, str]:
        async with httpx.AsyncClient() as c:
            ok_q, qvecs, d1 = await embed(c, queries, SF_DIMS)
            ok_c, cvecs, d2 = await embed(c, corpus, SF_DIMS)
            if not (ok_q and ok_c and qvecs and cvecs):
                return False, f"{d1} / {d2}"
            qm = np.asarray(qvecs, dtype=np.float32)
            cm = np.asarray(cvecs, dtype=np.float32)
            qm /= (np.linalg.norm(qm, axis=1, keepdims=True) + 1e-9)
            cm /= (np.linalg.norm(cm, axis=1, keepdims=True) + 1e-9)
            sims = qm @ cm.T
            top = sims.argmax(axis=1).tolist()
            return top == expected, f"向量召回 top1={top} 期望={expected}"

    ok, detail = await run()
    record("向量中文召回", ok, detail)


def citation_roundtrip() -> None:
    doc_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

    def cite_id(doc_hash: str, line_start: int, line_end: int) -> str:
        return f"{doc_hash}:{line_start}-{line_end}"

    def parse(citation: str) -> tuple[str, int, int] | None:
        try:
            h, rng = citation.split(":", 1)
            s, e = rng.split("-", 1)
            if len(h) != 64 or int(s) < 1 or int(e) < int(s):
                return None
            return h, int(s), int(e)
        except ValueError:
            return None

    cid = cite_id(doc_hash, 12, 15)
    ok = parse(cid) == (doc_hash, 12, 15) and parse("bad-input") is None
    record("引用确定性 ID", ok, f"生成并解析 {cid[:20]}…:12-15")


async def main(output: Path | None = None) -> None:
    print(f"模型: {GEMINI_MODEL} / {SF_MODEL}（密钥不打印）")
    async with httpx.AsyncClient() as client:
        for check in (gemini_basic, gemini_function_call, gemini_stream, embedding_dims):
            try:
                await check(client)
            except Exception as exc:
                record(check.__name__, False, f"异常 {type(exc).__name__}")
    fts_jieba()
    citation_roundtrip()
    try:
        await vector_recall()
    except Exception as exc:
        record("向量中文召回", False, f"异常 {type(exc).__name__}")

    lines = ["# 兼容性验证结果", "", f"运行时间：{time.strftime('%Y-%m-%d %H:%M:%S')}", ""]
    for r in RESULTS:
        lines.append(f"- [{'PASS' if r['ok'] else 'FAIL'}] **{r['step']}** — {r['detail']}")
    out = output or REPO / ".local-acceptance" / "兼容性验证.md"
    out.parent.mkdir(exist_ok=True)
    out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"\n结果已写入 {out}")
    failed = [r["step"] for r in RESULTS if not r["ok"]]
    if failed:
        print("未通过项：" + "、".join(failed))
        sys.exit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, help="结果文件；默认写入忽略目录 .local-acceptance")
    args = parser.parse_args()
    asyncio.run(main(args.output))
