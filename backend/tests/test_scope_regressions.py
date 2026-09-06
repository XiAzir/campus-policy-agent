import asyncio
import json
import sys
from pathlib import Path

import numpy as np
import pytest

from app import api, ingest
from app.agent import Agent, TurnScope
from app.audience import match_audience
from app.retrieval import allowed_doc_ids, search
from conftest import make_package


@pytest.fixture
def agent(db, vectors, monkeypatch):
    pid = ingest.import_package(db, make_package(), "scope.zip")
    ingest.publish_package(db, pid, {})
    value = Agent(db, vectors)
    async def embed(*args, **kwargs):
        vec = np.zeros(1024, dtype=np.float32)
        vec[0] = 1
        return vec
    monkeypatch.setattr("app.agent.embed_query", embed)
    monkeypatch.setattr(api, "db", db, raising=False)
    yield value
    asyncio.run(value.close())


def test_auto_past_includes_old_and_tool_chain_has_ids(agent):
    doc = agent.db.one("SELECT * FROM documents LIMIT 1")
    scope, _ = api._turn_scope(api.ScopeSpec(mode="auto"))
    with agent.db.tx() as conn:
        conn.execute("UPDATE documents SET deactivated_kind='superseded' WHERE id=?", (doc["id"],))
    async def run():
        evidence = []
        result = await agent.tool_policy_search({"query": "三下乡", "year_mode": "past"}, scope, evidence)
        hit = next(h for h in result["results"] if h["doc_uid"] == doc["doc_uid"])
        assert "audience" in hit and "effective_date" in hit
        source = await agent.tool_read_source(hit, scope, evidence)
        assert source["doc_uid"] == hit["doc_uid"] and source["lines"]
        versions = await agent.tool_get_versions(hit, scope, evidence)
        assert versions["versions"][0]["doc_uid"] == hit["doc_uid"]
    asyncio.run(run())


def test_search_stages_follow_real_embedding_and_failure(agent, monkeypatch):
    from app.llm import LLMError

    events = []

    async def emit(event):
        events.append(event)

    async def fail_embed(*args):
        assert events == [{"event": "stage", "stage": "embedding"}]
        raise LLMError("测试上游失败")

    monkeypatch.setattr("app.agent.embed_query", fail_embed)
    with pytest.raises(LLMError):
        asyncio.run(agent.tool_policy_search({"query": "三下乡"}, TurnScope(), [], emit))
    assert events == [{"event": "stage", "stage": "embedding"},
                      {"event": "stage", "stage": "embedding", "status": "failed"}]


def test_no_tool_call_does_not_claim_retrieval(agent):
    events = []

    class NoTools:
        async def stream(self, *args, **kwargs):
            yield {"type": "text", "text": "请补充问题"}
            yield {"type": "parts", "parts": [{"text": "请补充问题"}]}

    async def run():
        original = agent.gemini
        agent.gemini = NoTools()
        async def emit(event):
            events.append(event)
        try:
            await agent.run([], "你好", TurnScope(), {}, emit)
        finally:
            agent.gemini = original

    asyncio.run(run())
    assert "retrieving" not in [event["event"] for event in events]
    assert [event["stage"] for event in events if event["event"] == "stage"] == ["analyzing", "verifying"]


@pytest.mark.parametrize("during_embed", [False, True])
def test_manual_disable_rechecked_for_queued_file_request(agent, monkeypatch, during_embed):
    doc = agent.db.one("SELECT * FROM documents LIMIT 1")
    scope, _ = api._turn_scope(api.ScopeSpec(mode="files", doc_uids=[doc["doc_uid"]]))
    if during_embed:
        async def embed(*args):
            ingest.deactivate_document(agent.db, doc["doc_uid"])
            return np.ones(1024, dtype=np.float32) / 32
        monkeypatch.setattr("app.agent.embed_query", embed)
    else:
        ingest.deactivate_document(agent.db, doc["doc_uid"])
    result = asyncio.run(agent.tool_policy_search({"query": "三下乡"}, scope, []))
    assert result["results"] == []


def test_audience_filters_college_year_and_unknown(agent):
    doc = agent.db.one("SELECT * FROM documents LIMIT 1")
    with agent.db.tx() as conn:
        conn.execute("UPDATE documents SET audience_scope=? WHERE id=?", (json.dumps({"confirmed": True, "colleges": ["信息工程学院"], "entry_years": ["2024"]}), doc["id"]))
    assert doc["id"] not in allowed_doc_ids(agent.db, profile={"college": "其他学院", "entry_year": "2024"})
    assert doc["id"] not in allowed_doc_ids(agent.db, profile={"college": "信息工程学院"})
    assert doc["id"] in allowed_doc_ids(agent.db, profile={"college": "信息工程学院", "entry_year": "2024"})
    scope = TurnScope(strict_files=True, strict_doc_uids=[doc["doc_uid"]], profile={})
    result = asyncio.run(agent.tool_policy_search({"query": "三下乡"}, scope, []))
    assert "学院" in result["clarification_required"] and not result["results"]
    assert not match_audience({"audience": ["部分学生"], "audience_scope": {}}, {})[0]


def test_section_filter_applies_to_fts_vectors_and_source_read(agent):
    db = agent.db
    doc = db.one("SELECT * FROM documents LIMIT 1")
    with db.tx() as conn:
        conn.execute("UPDATE sections SET end_line=1 WHERE doc_id=?", (doc["id"],))
        conn.execute("UPDATE doc_tags SET section_id='s1' WHERE doc_id=?", (doc["id"],))
        conn.execute("UPDATE chunks SET line_start=2,line_end=3 WHERE doc_id=?", (doc["id"],))
    scope = {doc["id"]}
    for vector in (None, np.ones(1024, dtype=np.float32)):
        assert search(db, agent.vectors, "社会实践", vector, scope, domains=["共青团"]) == []
    turn = TurnScope(domains=["共青团"])
    result = asyncio.run(agent.tool_read_source({"doc_uid": doc["doc_uid"], "line_start": 2, "line_end": 3}, turn, []))
    assert "error" in result


def test_domain_expansion_requires_prior_search_and_reason(agent):
    scope = TurnScope(domains=["没有资料的领域"])
    async def run():
        assert "error" in await agent.tool_policy_search({"query": "社会实践", "expand_domains": True, "reason": "需要跨领域"}, scope, [])
        assert not (await agent.tool_policy_search({"query": "社会实践"}, scope, []))["results"]
        result = await agent.tool_policy_search({"query": "社会实践", "expand_domains": True, "reason": "需要共青团条款"}, scope, [])
        assert result["results"] and result["expanded_reason"] == "需要共青团条款"
    asyncio.run(run())


def test_manual_superseded_old_cannot_be_enabled(agent):
    db = agent.db
    old = db.one("SELECT * FROM documents LIMIT 1")
    ingest.deactivate_document(db, old["doc_uid"])
    pid = ingest.import_package(db, make_package("新版"), "new.zip")
    h = ingest.preview_package(db, pid)["documents"][0]["doc_hash"]
    ingest.publish_package(db, pid, {h: old["doc_uid"]})
    with pytest.raises(ingest.IngestError, match="解除替代关系"):
        ingest.enable_document(db, old["doc_uid"])
    newer = db.one("SELECT doc_uid FROM documents WHERE replaces_doc_id=?", (old["id"],))
    ingest.unlink_replacement(db, newer["doc_uid"])
    ingest.enable_document(db, old["doc_uid"])


def test_preprocessor_chunks_do_not_cross_sections():
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "skill" / "scripts"))
    from common import chunk_lines
    lines = ["a" * 300, "b" * 300, "c" * 300, "d" * 300]
    sections = [{"start_line": 1, "end_line": 2}, {"start_line": 3, "end_line": 4}]
    chunks = chunk_lines(lines, sections)
    assert all(not (c["line_start"] <= 2 < c["line_end"]) for c in chunks)
    assert all(len(c["text"]) <= 800 for c in chunks)
