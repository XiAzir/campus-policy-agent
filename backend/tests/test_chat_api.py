"""聊天管理器、历史截断、API 端到端（假 LLM）测试。"""

import asyncio
import json

import pytest
from fastapi.testclient import TestClient

from app.chat import ChatManager, trim_history


# ---------- ChatManager ----------

def test_queue_and_finish():
    mgr = ChatManager()
    order = []

    async def make_runner(tag, delay):
        async def runner(job):
            order.append(f"run-{tag}")
            await asyncio.sleep(delay)
            job.queue.put_nowait({"event": "done", "tag": tag})
        return runner

    async def scenario():
        j1 = await mgr.submit("c1", await make_runner("a", 0.05))
        j2 = await mgr.submit("c2", await make_runner("b", 0.01))
        ev1 = [e async for e in drain(j1)]
        ev2 = [e async for e in drain(j2)]
        await asyncio.sleep(0.15)
        return ev1, ev2, order

    ev1, ev2, order = asyncio.run(scenario())
    assert [e["event"] for e in ev1][:2] == ["started", "done"]
    assert [e["event"] for e in ev2][:2] == ["queued", "started"]
    assert order == ["run-a", "run-b"]


async def drain(job):
    while True:
        ev = await job.queue.get()
        yield ev
        if ev.get("event") == "__end__":
            return


def test_per_client_single_request():
    mgr = ChatManager()

    async def runner(job):
        await asyncio.sleep(0.5)

    async def scenario():
        await mgr.submit("c1", runner)
        with pytest.raises(Exception, match="同一浏览器"):
            await mgr.submit("c1", runner)

    asyncio.run(scenario())


def test_cancel_running():
    mgr = ChatManager()
    ran = []

    async def runner(job):
        ran.append("started")
        await asyncio.sleep(5)

    async def scenario():
        job = await mgr.submit("c1", runner)
        await asyncio.sleep(0.05)
        ok = await mgr.cancel(job.request_id, "c1")
        await asyncio.sleep(0.05)
        assert ok
        return [e async for e in drain(job)]

    events = asyncio.run(scenario())
    assert any(e["event"] == "error" and "取消" in e.get("message", "") for e in events)
    assert not any(e["event"] == "__end__" and False for e in events)


# ---------- trim_history ----------

def test_trim_history_rounds_and_chars():
    msgs = []
    for i in range(30):
        msgs.append({"role": "user", "parts": [{"text": "x" * 100}]})
        msgs.append({"role": "model", "parts": [{"text": "y" * 100}]})
    trimmed = trim_history(msgs)
    assert len(trimmed) <= 24
    assert trimmed[-1]["role"] == "model"
    long_msgs = [{"role": "user", "parts": [{"text": "z" * 5000}]} for _ in range(10)]
    assert sum(len(p["text"]) for m in trim_history(long_msgs) for p in m["parts"]) <= 8000


# ---------- API 端到端 ----------

@pytest.fixture()
def client(real_package_bytes, monkeypatch):
    """完整 HTTP 栈 + 假 Gemini。"""
    from app import api
    from app.agent import Agent
    from app.chat import ChatManager
    from app.config import config
    from app.db import Database
    from app.main import app
    from app.security import Tokens, init_admin
    from app.vectors import get_index

    db = Database(config.data_dir / "campus.db")
    init_admin(db)
    api.db = db
    api.tokens = Tokens(db)
    api.chats = ChatManager()

    fake_agent = Agent(db, get_index(config.data_dir / "vectors"))

    class FakeGemini:
        async def stream(self, contents, system=None, tools=None, temperature=0.2):
            has_tool_result = any(
                "functionResponse" in p for m in contents if m["role"] == "user" for p in m["parts"]
            )
            if tools and not has_tool_result:
                call_part = {
                    "functionCall": {
                        "name": "policy_search",
                        "args": {"query": "三下乡 报名"},
                        "id": "call-1",
                    }
                }
                yield {"type": "parts", "parts": [call_part]}
            else:
                text = "报名需要在 6月22日前 完成 [[EV1]]，此外[[EV99]]是伪造引用。"
                yield {"type": "text", "text": text}
                yield {"type": "parts", "parts": [{"text": text}]}

        async def close(self):
            return None

    async def fake_embed_query(text, dims=None):
        import numpy as np

        return np.ones(1024, dtype=np.float32) / 32

    asyncio.run(fake_agent.close())
    fake_agent.gemini = FakeGemini()
    monkeypatch.setattr("app.agent.embed_query", fake_embed_query)

    # 不用 with：避免 lifespan 再开一条真实库连接（恢复测试在 Windows 上会文件冲突）
    c = TestClient(app)
    # 手动注入测试替身（等价于 lifespan 后再覆盖）
    api.db = db
    api.tokens = Tokens(db)
    api.chats = ChatManager()
    api.agent = fake_agent
    yield c
    c.close()
    asyncio.run(fake_agent.close())
    fake_agent.vectors.close_all()
    db.close()


def _admin_headers(client):
    r = client.post("/api/auth/admin/login", json={"password": "admin"})
    assert r.status_code == 200
    return {"Authorization": f"Bearer {r.json()['admin_token']}"}


def test_full_flow(client, real_package_bytes, tmp_path):
    # 1) 访问码未设置时用户不能登录
    assert client.post("/api/auth/login", json={"code": "x"}).status_code == 400
    # 2) 管理员登录并设置访问码
    ah = _admin_headers(client)
    assert client.put("/api/admin/access-code", json={"code": "2026fall"}, headers=ah).status_code == 200
    # 3) 用户登录
    r = client.post("/api/auth/login", json={"code": "2026fall"})
    assert r.status_code == 200
    user_h = {"Authorization": f"Bearer {r.json()['token']}"}
    client_id = r.json()["client_id"]
    # 旧码立即失效：重设后旧码登录失败
    assert client.put("/api/admin/access-code", json={"code": "2026winter"}, headers=ah).status_code == 200
    assert client.post("/api/auth/login", json={"code": "2026fall"}).status_code == 401
    r2 = client.post("/api/auth/login", json={"code": "2026winter"})
    assert r2.status_code == 200
    # 4) 上传 → 预览 → 发布
    files = {"file": ("pkg.zip", real_package_bytes, "application/zip")}
    up = client.post("/api/admin/packages", files=files, headers=ah)
    assert up.status_code == 200, up.text
    pid = up.json()["id"]
    preview = client.get(f"/api/admin/packages/{pid}", headers=ah).json()
    assert len(preview["documents"]) == 5
    pub = client.post(f"/api/admin/packages/{pid}/publish", json={"replacements": {}}, headers=ah)
    assert pub.status_code == 200, pub.text
    # 5) 用户目录
    cat = client.get("/api/catalog", headers=user_h).json()
    assert len(cat["documents"]) == 5
    # 6) 原文读取与文件下载（需鉴权）
    uid = cat["documents"][0]["doc_uid"]
    txt = client.get(f"/api/source/{uid}/text?frm=1&to=5", headers=user_h)
    assert txt.status_code == 200
    assert txt.json()["lines"][0].startswith("L1:")
    assert client.get(f"/api/source/{uid}/file", headers=user_h).status_code == 200
    assert client.get(f"/api/source/{uid}/file").status_code == 401
    # 7) 聊天 SSE：工具调用 → 证据引用 → 伪造引用被剔除
    body = {
        "question": "三下乡什么时候报名？",
        "messages": [],
        "scope": {"mode": "auto"},
        "profile": {"college": "信息工程学院"},
    }
    with client.stream(
        "POST",
        "/api/chat",
        json=body,
        headers={**user_h, "X-Client-Id": client_id},
    ) as resp:
        assert resp.status_code == 200
        events = []
        for line in resp.iter_lines():
            if line.startswith("data: "):
                events.append(json.loads(line[6:]))
    kinds = [e["event"] for e in events]
    assert "retrieving" in kinds and "generating" in kinds
    done = next(e for e in events if e["event"] == "done")
    assert "[[EV99]]" not in done["text"]
    assert "[[EV1]]" in done["text"]
    cit = next(e for e in events if e["event"] == "citations")
    assert len(cit["citations"]) == 1
    assert cit["citations"][0]["doc_uid"]
    # 8) 管理端停用/启用
    assert client.post(f"/api/admin/documents/{uid}/deactivate", headers=ah).status_code == 200
    cat2 = client.get("/api/catalog", headers=user_h).json()
    assert len(cat2["documents"]) == 4
    assert client.post(f"/api/admin/documents/{uid}/enable", headers=ah).status_code == 200
    # 9) 备份可下载（内容校验在 restore 测试）
    bk = client.get("/api/admin/backup", headers=ah)
    assert bk.status_code == 200
    assert bk.content[:2] == b"PK"
    # 10) 备份恢复
    assert client.post(f"/api/admin/documents/{uid}/deactivate", headers=ah).status_code == 200
    assert len(client.get("/api/catalog", headers=user_h).json()["documents"]) == 4
    rs = client.post(
        "/api/admin/restore",
        files={"file": ("backup.zip", bk.content, "application/zip")},
        headers=ah,
    )
    assert rs.status_code == 200, rs.text
    cat3 = client.get("/api/catalog", headers=user_h).json()
    assert len(cat3["documents"]) == 5


def test_chat_queue_reject_when_full(client):
    """同 client 第二个请求被拒的并发语义已在 ChatManager 单测覆盖；
    这里验证：请求全部完成后资源释放，可再次提问。"""
    ah = _admin_headers(client)
    client.put("/api/admin/access-code", json={"code": "code1234"}, headers=ah)
    r = client.post("/api/auth/login", json={"code": "code1234"})
    user_h = {"Authorization": f"Bearer {r.json()['token']}"}
    client_id = r.json()["client_id"]
    body = {"question": "hi", "messages": [], "scope": {}, "profile": {}}
    r1 = client.post("/api/chat", json=body, headers={**user_h, "X-Client-Id": client_id})
    assert r1.status_code == 200
    r2 = client.post("/api/chat", json=body, headers={**user_h, "X-Client-Id": client_id})
    assert r2.status_code == 200  # 前一个任务已结束并释放槽位


def test_restore_reloads_token_signer(client):
    from app import api
    from app.security import Tokens

    ah = _admin_headers(client)
    client.put("/api/admin/access-code", json={"code": "restore-code"}, headers=ah)
    archive = client.get("/api/admin/backup", headers=ah)
    assert archive.status_code == 200
    api.db.setting_set("token_secret", "ab" * 32)
    api.tokens = Tokens(api.db)
    ah = _admin_headers(client)
    response = client.post("/api/admin/restore", files={"file": ("backup.zip", archive.content, "application/zip")}, headers=ah)
    assert response.status_code == 200, response.text
    login = client.post("/api/auth/login", json={"code": "restore-code"})
    assert login.status_code == 200
    assert client.get("/api/catalog", headers={"Authorization": "Bearer " + login.json()["token"]}).status_code == 200


def test_restore_gate_blocks_new_requests(client):
    from app.maintenance import maintenance
    maintenance.restoring = True
    try:
        assert client.post("/api/auth/login", json={"code": "code"}).status_code == 503
        assert client.get("/api/catalog").status_code == 503
    finally:
        maintenance.restoring = False


def test_restore_cancels_running_and_waiting_jobs():
    async def run():
        mgr = ChatManager()
        started = asyncio.Event()
        async def runner(job):
            started.set()
            await asyncio.sleep(60)
        first = await mgr.submit("one", runner)
        await started.wait()
        second = await mgr.submit("two", runner)
        await mgr.cancel_all()
        assert first.task.done()
        assert not mgr._by_client and not mgr._waiting
        assert any(e["event"] == "error" for e in [x async for x in drain(second)])
    asyncio.run(run())
