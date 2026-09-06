"""M0 契约夹具生成器：以 Python 基线实现为准，冻结 Rust 必须互认的契约。

运行环境：仓库根目录 .venv（含 fastapi/jieba/numpy）。产物写入
backend-rust/tests/fixtures/，只含小型合成数据，不含密钥与私有资料。

夹具分两类：
- 互认向量（token、口令哈希、jieba 分词、FTS bm25、向量点积）——Rust 实现必须逐值对齐；
- 路由/SSE 快照——以单次生成运行的真实响应冻结，动态字段（时间、耗时、随机 id）
  在生成时刻取值后不再变化，Rust 测试只比较结构与非动态字段。
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import hmac
import importlib
import io
import json
import os
import re
import sqlite3
import sys
import tempfile
import time
import uuid
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "backend"))
FIXTURES = REPO / "backend-rust" / "tests" / "fixtures"

# 与 backend/tests/conftest.py 相同的假配置；不读取真实 .env 中的密钥。
os.environ["DATA_DIR"] = tempfile.mkdtemp(prefix="cpb-fixture-")
os.environ["GEMINI_BASE_URL"] = "https://fake.local/v1beta"
os.environ["GEMINI_API_KEY"] = "fake-key-not-real"
os.environ["GEMINI_MODEL"] = "fake-model"
os.environ["SILICONFLOW_API_KEY"] = "fake-sf-key"
os.environ["SILICONFLOW_EMBED_MODEL"] = "test-embed"
os.environ["INITIAL_ADMIN_PASSWORD"] = "admin"

import numpy as np  # noqa: E402
import jieba  # noqa: E402

from app import api as app_api  # noqa: E402
from app import db as db_mod  # noqa: E402
from app import ingest as ingest_mod  # noqa: E402
from app import backup as backup_mod  # noqa: E402
from app.config import config  # noqa: E402
from app.db import Database  # noqa: E402
from app.security import Tokens, hash_password, limiter  # noqa: E402
from app.textutil import fts_match_query, tokenize_for_fts  # noqa: E402

FIXED_NOW = "2026-09-06T12:00:00Z"


def _freeze_time():
    """把所有以 `from .db import utcnow` 引入的调用点固定为同一时刻，保证快照可复现。"""
    for name, mod in (("app.db", db_mod), ("app.ingest", ingest_mod), ("app.api", app_api), ("app.backup", backup_mod)):
        if hasattr(mod, "utcnow"):
            mod.utcnow = lambda: FIXED_NOW


_write_count = 0


def write_json(rel: str, data) -> None:
    global _write_count
    path = FIXTURES / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    _write_count += 1


def write_bytes(rel: str, data: bytes) -> None:
    global _write_count
    path = FIXTURES / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    _write_count += 1


# ---------- 1. 鉴权互认向量 ----------

TOKEN_SECRET_HEX = "3f2a" * 16  # 32 字节固定密钥，仅用于夹具


def gen_token_vectors():
    cases = []
    payloads = [
        {"r": "user", "c": "0f3d9a2b7c6145e8a1b2c3d4e5f60718", "e": 1893456000},
        {"r": "admin", "c": "admin-client-0001", "e": 1893456000},
        {"r": "user", "c": "expired", "e": 1000000000},
    ]
    key = bytes.fromhex(TOKEN_SECRET_HEX)
    for p in payloads:
        raw = base64.urlsafe_b64encode(json.dumps(p).encode()).decode().rstrip("=")
        sig = hmac.new(key, raw.encode(), hashlib.sha256).hexdigest()[:32]
        cases.append({
            "payload": p, "raw": raw, "sig": sig, "token": f"{raw}.{sig}",
            "role": p["r"], "client_id": p["c"],
            "expired": p["e"] < 1700000000,
        })
    # 同一 payload、不同 JSON 序列化（空格/键序）必须验签一致：验证用原始段，不重序列化。
    p = payloads[0]
    raw_spaced = base64.urlsafe_b64encode(
        json.dumps({"c": p["c"], "r": p["r"], "e": p["e"]}, separators=(", ", ": ")).encode()
    ).decode().rstrip("=")
    sig_spaced = hmac.new(key, raw_spaced.encode(), hashlib.sha256).hexdigest()[:32]
    cases.append({
        "payload": p, "raw": raw_spaced, "sig": sig_spaced, "token": f"{raw_spaced}.{sig_spaced}",
        "role": "user", "client_id": p["c"], "expired": False, "note": "键序与空格不同的等价 payload",
    })
    # 篡改签名必须拒绝
    bad = cases[0]["token"][:-1] + ("0" if cases[0]["token"][-1] != "0" else "1")
    write_json("auth/token_vectors.json", {
        "secret_hex": TOKEN_SECRET_HEX,
        "note": "签名为对原始 base64url 段做 HMAC-SHA256 后取 hex 前 32 字符；payload 无 padding",
        "cases": cases, "tampered_token": bad, "tampered_valid": False,
    })


def gen_password_vectors():
    cases = []
    for password, salt_hex in [
        ("admin", "00112233445566778899aabbccddeeff"),
        ("2026fall", "a1b2c3d4e5f60718293a4b5c6d7e8f90"),
        ("访客访问码With中文", "deadbeefcafebabe0123456789abcdef"),
        ("x" * 128, "ffffffffeeeeeeeeddddddddcccccccc"),
    ]:
        stored = hash_password(password, salt=bytes.fromhex(salt_hex))
        cases.append({
            "password": password, "salt_hex": salt_hex, "stored": stored, "verify_ok": True,
            "wrong_password_verify": hash_password("definitely-wrong", salt=bytes.fromhex(salt_hex)) != stored,
        })
    write_json("auth/password_vectors.json", {
        "algorithm": "PBKDF2-HMAC-SHA256", "iterations": 200000, "salt_bytes": 16, "output_bytes": 32,
        "format": "pbkdf2$<salt hex>$<digest hex>", "cases": cases,
        "malformed": ["", "pbkdf2$zz$00", "plain$0011$aa", "pbkdf2$0011"],
    })


# ---------- 2. jieba 分词 golden ----------

CORPUS = [
    "三下乡社会实践报名截止日期为6月22日。",
    "武汉理工大学社会实践基地管理办法（试行）",
    "信息工程学院2025级学生须在6月30日前提交材料！",
    "管理学院与经济学院联合开展暑期社会实践，欢迎全校同学报名。",
    "图书馆WiFi6改造工程施工期间闭馆，2024年入学的本科生注意查收通知。",
    "奖学金评定办法：GPA排名前10%可申请一等，其余同学可申请二等。",
    "宿舍违规电器处理规定（2025年修订）， including 电煮锅、电热毯等大功率电器。",
    "青苗计划是2026年新设立的创新创业训练项目。",
    "校纪处分申诉流程：学生可在收到处分决定书之日起10个工作日内提出申诉，逾期不予受理。",
    "--------------------------------------------------",
    "标点测试：句号、逗号，分号；顿号、感叹号！问号？引号“引号”括号（括号）冒号：",
    "数字与单位混合：2026年9月6日，第3食堂，A栋105室，占全校师生总数的85%以上。",
    "英文与中文混排：请使用PDF格式提交，文件命名格式为“学号-姓名-report”。",
    "重复词重复词重复词测试， jjieba 未登录词测试 zz unseen。",
    "很长的一个专有名词：大学生创新创业训练计划国家级重点项目立项管理办法",
]


def gen_jieba_golden():
    jieba.initialize()
    corpus = []
    for text in CORPUS:
        corpus.append({
            "text": text,
            "search_tokens": [t for t in jieba.cut_for_search(text)],
            "fts_index_body": tokenize_for_fts(text),
            "fts_match_query": fts_match_query(text),
            "fts_match_query_max8": fts_match_query(text, max_tokens=8),
        })
    # 查询串去重、截断与符号过滤
    queries = [
        "三下乡 报名 时间",
        "报名！报名，报名；报名",
        "！！！！！？？？？",
        "宿舍 电煮锅 处分",
        "奖学金 GPA 排名 2025级",
        "a" * 100,
        "“引号”*token{}test",
    ]
    qcases = [{"text": q, "match": fts_match_query(q)} for q in queries]
    dict_path = Path(jieba.__file__).parent / "dict.txt"
    dict_hash = hashlib.sha256(dict_path.read_bytes()).hexdigest() if dict_path.exists() else None
    write_json("tokenize/jieba_golden.json", {
        "jieba_version": importlib.metadata.version("jieba") if importlib.metadata else None,
        "dict_sha256": dict_hash,
        "dict_freq_entries": len(jieba.dt.FREQ),
        "mode": "cut_for_search",
        "note": "Rust 侧必须逐 token 对齐（顺序与重复 token）；写入侧用空格连接，查询侧去重后最多 24 个 token、双引号包装、OR 连接",
        "corpus": corpus,
        "queries": qcases,
    })


def gen_fts_bm25_golden():
    """冻结 bm25 分值方向与数值：同一 FTS5 库上 Rust 查询必须得到相同排序与近似分值。"""
    conn = sqlite3.connect(":memory:")
    conn.execute("CREATE VIRTUAL TABLE chunks_fts USING fts5(body)")
    bodies = [tokenize_for_fts(t) for t in CORPUS[:12]]
    for i, body in enumerate(bodies, 1):
        conn.execute("INSERT INTO chunks_fts(rowid, body) VALUES(?,?)", (i, body))
    conn.commit()
    queries = [
        ("社会实践 报名", [1, 2, 4]),
        ("宿舍 违规电器 处分", [7]),
        ("奖学金 GPA", [6]),
        ("青苗 计划", [8]),
        ("申诉 工作日", [9]),
    ]
    cases = []
    for query, _expected in queries:
        match = fts_match_query(query)
        rows = conn.execute(
            "SELECT rowid, bm25(chunks_fts) AS score FROM chunks_fts WHERE chunks_fts MATCH ? ORDER BY score",
            (match,),
        ).fetchall()
        cases.append({
            "query": query, "match": match,
            "hits": [{"rowid": r, "bm25": r_score} for r, r_score in rows],
        })
    write_json("retrieval/fts_bm25_golden.json", {
        "sqlite_version": sqlite3.sqlite_version,
        "note": "bm25 分值为 FTS5 默认参数；排序必须一致，分值方向为负值越大越相关（ORDER BY score 升序）",
        "cases": cases,
    })
    conn.close()


def gen_vector_golden():
    """冻结 float32 点积数值与 Fortran 布局语义。dim=8 便于人工核对。"""
    rng = np.random.default_rng(20260906)
    mat = rng.standard_normal((16, 8)).astype(np.float32)
    mat /= np.linalg.norm(mat, axis=1, keepdims=True)
    query = rng.standard_normal(8).astype(np.float32)
    query /= np.linalg.norm(query)
    scores = (mat @ query).astype(np.float32)
    fortran = np.asfortranarray(mat)
    buf_c, buf_f = io.BytesIO(), io.BytesIO()
    np.save(buf_c, mat)
    np.save(buf_f, fortran)
    write_bytes("retrieval/vectors_dim8_c.npy", buf_c.getvalue())
    write_bytes("retrieval/vectors_dim8_fortran.npy", buf_f.getvalue())
    # 拒绝样本：错误 dtype、一维、含 NaN、零行、大端
    bad = {}
    m64 = mat.astype(np.float64)
    b = io.BytesIO(); np.save(b, m64); bad["dtype_float64"] = b.getvalue()
    b = io.BytesIO(); np.save(b, mat[0]); bad["ndim_1"] = b.getvalue()
    m_nan = mat.copy(); m_nan[3, 2] = np.nan; b = io.BytesIO(); np.save(b, m_nan); bad["has_nan"] = b.getvalue()
    m_zero = mat.copy(); m_zero[5] = 0; b = io.BytesIO(); np.save(b, m_zero); bad["zero_row"] = b.getvalue()
    m_be = mat.byteswap().view(mat.dtype.newbyteorder(">")); b = io.BytesIO(); np.save(b, m_be); bad["big_endian"] = b.getvalue()
    for name, data in bad.items():
        write_bytes(f"retrieval/bad_{name}.npy", data)
    write_json("retrieval/vector_queries.json", {
        "query": [float(x) for x in query],
        "query_norm": float(np.linalg.norm(query)),
        "dim": 8, "rows": 16,
        "expected_scores_f32": [float(np.float32(x)) for x in scores],
        "note": "点积按 float32 计算，允许绝对误差 1e-5；Fortran 文件与 C 文件逻辑内容相同，得分必须一致",
    })


# ---------- 3. 配置默认值 ----------

def gen_config_defaults():
    write_json("config/config_defaults.json", {
        "token_ttl_days": 30,
        "chat_concurrency": 1,
        "chat_queue_max": 10,
        "chat_history_max_rounds": 12,
        "chat_history_max_chars": 8000,
        "chat_request_timeout_s": 180,
        "chat_max_tool_rounds": 3,
        "max_package_mb": 200,
        "disk_warn_gb": 8.0,
        "max_upload_body_mb": 210,
        "embed_dims": 1024,
        "preprocessing_version": "v1",
        "host": "127.0.0.1",
        "port": 8000,
        "initial_admin_password": "admin",
        "note": "环境变量可覆盖（GEMINI_BASE_URL/GEMINI_API_KEY/GEMINI_MODEL/SILICONFLOW_* 为必填）",
    })


# ---------- 4. 资料包夹具 ----------

def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def long_policy_text(title: str, lines: int) -> str:
    rows = [title]
    for i in range(1, lines):
        rows.append(f"第{i}条 本办法第{i}章第{i}节适用于全校学生，自发布之日起施行。")
    return "\n".join(rows) + "\n"


def build_small_package() -> tuple[bytes, dict]:
    """6 份合成文档：2 txt、1 md、1 pdf（含 page_map）、1 docx、1 长文（250 行）。"""
    dim = 1024
    docs_spec = [
        ("三下乡报名通知", "txt", "txt", "团委", None, None),
        ("社会实践安全须知", "txt", "txt", "保卫处", None, None),
        ("奖学金评定办法", "md", "md", "学生工作处", None, None),
        ("社会实践基地管理办法（试行）", "pdf", "pdf", "教务处", [[1, 1], [30, 2], [60, 3]], None),
        ("宿舍管理规定（修订版）", "docx", "docx", "学生工作处", None, None),
        ("校纪处分条例（长文样本）", "txt", "txt", "学生工作处", None, 250),
    ]
    documents, entries = [], {}
    for i, (title, ftype, ext, dept, page_map, line_count) in enumerate(docs_spec):
        if line_count and line_count > 5:
            text = long_policy_text(title, line_count)
        else:
            text = (
                f"{title}\n"
                f"第一条 {title}适用于全校在读学生，自2026年6月1日起施行。\n"
                f"第二条 违反本规定的，视情节给予批评教育或相应处分。\n"
                f"第三条 本办法由{dept}负责解释。\n"
            )
        blob = text.encode()
        h = sha256_bytes(blob)
        n_lines = text.count("\n")
        documents.append({
            "doc_hash": h, "original_filename": f"document-{i}.{ext}", "doc_type": ftype,
            "title": title, "department": dept, "effective_date": "2026-06-01",
            "audience": ["全校"], "audience_scope": {},
            "domains": [{"tag": "学生事务" if i % 2 == 0 else "安全纪律", "section_ids": ["s1"]}],
            "replaces": None, "notes": "", "line_count": n_lines, "page_map": page_map,
            "sections": [{"section_id": "s1", "title": title, "start_line": 1, "end_line": n_lines}],
            "text_sha256": h,
            "chunks": [{"vector_index": i, "text": text.rstrip("\n"), "line_start": 1,
                         "line_end": n_lines, "section_id": "s1"}],
        })
        entries[f"files/{h}.{ext}"] = blob
        entries[f"text/{h}.txt"] = blob
    vecs = np.zeros((len(documents), dim), dtype=np.float32)
    for i in range(len(documents)):
        vecs[i, i % dim] = 1.0
    buf = io.BytesIO(); np.save(buf, vecs); entries["vectors.npy"] = buf.getvalue()
    manifest = {
        "format_version": 1, "preprocessing_version": "v1", "embed_model": "test-embed",
        "embed_dim": dim, "documents": documents,
        "vectors": {"file": "vectors.npy", "dtype": "float32", "count": len(documents), "dim": dim},
    }
    entries["manifest.json"] = json.dumps(manifest, ensure_ascii=False).encode()
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return out.getvalue(), manifest


def gen_bad_package_traversal() -> bytes:
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w") as zf:
        zf.writestr("../evil.txt", "x")
    return out.getvalue()


# ---------- 5. 路由快照 ----------

def sanitize_headers(headers) -> dict:
    keep = ["content-type", "x-content-type-options", "cache-control", "x-accel-buffering"]
    out = {}
    for k in keep:
        v = headers.get(k)
        if v is not None:
            out[k] = v
    cd = headers.get("content-disposition")
    if cd:
        out["content-disposition"] = cd
    return out


class RouteCapture:
    def __init__(self, outdir: str):
        self.outdir = outdir
        self.cases = []

    def record(self, name, method, path, status, request=None, response_headers=None, response_body=None, note=None):
        case = {"name": name, "method": method, "path": path, "status": status,
                "response_headers": response_headers or {}, "response_body": response_body}
        if request is not None:
            case["request"] = request
        if note:
            case["note"] = note
        write_json(f"routes/{self.outdir}_{name}.json", case)
        self.cases.append(name)


def drive_routes(client, pkg_bytes):
    from fastapi.testclient import TestClient  # noqa: F401  (仅说明依赖)

    cap = RouteCapture("main")
    from app.security import limiter

    # --- auth/state 与未设置访问码的登录 ---
    r = client.get("/api/auth/state")
    cap.record("auth_state_initial", "GET", "/api/auth/state", r.status_code, response_body=r.json())
    r = client.post("/api/auth/login", json={"code": "whatever"})
    cap.record("login_no_access_code", "POST", "/api/auth/login", r.status_code,
               request={"code": "whatever"}, response_body=r.json())
    r = client.post("/api/auth/login", json={})
    cap.record("login_missing_field", "POST", "/api/auth/login", r.status_code, request={}, response_body=r.json())

    # --- 管理员登录 ---
    r = client.post("/api/auth/admin/login", json={"password": "admin"})
    assert r.status_code == 200
    admin_token = r.json()["admin_token"]
    ah = {"Authorization": f"Bearer {admin_token}"}
    cap.record("admin_login_ok", "POST", "/api/auth/admin/login", r.status_code,
               request={"password": "(已隐去)"}, response_body={"admin_token": "<jwt 形如 base64url.sig>"})
    r = client.post("/api/auth/admin/login", json={"password": "wrong"})
    cap.record("admin_login_wrong", "POST", "/api/auth/admin/login", r.status_code,
               request={"password": "wrong"}, response_body=r.json())

    # --- 修改密码 ---
    r = client.post("/api/auth/admin/password", json={"old_password": "admin", "new_password": "short"})
    cap.record("admin_password_too_short", "POST", "/api/auth/admin/password", r.status_code,
               request={"old_password": "admin", "new_password": "short"}, response_body=r.json())
    r = client.post("/api/auth/admin/password", json={"old_password": "bad", "new_password": "newpass123"},
                    headers=ah)
    cap.record("admin_password_wrong_old", "POST", "/api/auth/admin/password", r.status_code,
               request={"old_password": "bad", "new_password": "newpass123"}, response_body=r.json())
    r = client.post("/api/auth/admin/password", json={"old_password": "admin", "new_password": "admin"})
    cap.record("admin_password_same", "POST", "/api/auth/admin/password", r.status_code,
               request={"old_password": "admin", "new_password": "admin"}, response_body=r.json())
    r = client.post("/api/auth/admin/password", json={"old_password": "admin", "new_password": "newpass123"},
                    headers=ah)
    cap.record("admin_password_ok", "POST", "/api/auth/admin/password", r.status_code,
               request={"old_password": "admin", "new_password": "newpass123"}, response_body=r.json())
    # 改回一个满足长度要求的密码（min_length=8，"admin" 不满足——这本身也是契约）
    r = client.post("/api/auth/admin/password", json={"old_password": "newpass123", "new_password": "admin1234"},
                    headers=ah)
    cap.record("admin_password_rotate_back", "POST", "/api/auth/admin/password", r.status_code,
               request={"old_password": "(当前密码)", "new_password": "admin1234"}, response_body=r.json(),
               note="new_password 最小 8 字符；初始密码 admin 仅用于空库初始化")

    # --- 上传资料包（错误样例在前） ---
    r = client.post("/api/admin/packages", files={"file": ("bad.zip", gen_bad_package_traversal(), "application/zip")},
                    headers=ah)
    cap.record("upload_traversal_rejected", "POST", "/api/admin/packages", r.status_code,
               request={"file": "(含 ../ 路径的 zip)"}, response_body=r.json())
    r = client.post("/api/admin/packages", headers=ah)
    cap.record("upload_missing_file", "POST", "/api/admin/packages", r.status_code, response_body=r.json())
    saved_limit = config.max_upload_body_mb
    config.max_upload_body_mb = 0  # 触发 413：请求体上限在 multipart 解析前生效
    r = client.post("/api/admin/packages", files={"file": ("pkg.zip", pkg_bytes, "application/zip")}, headers=ah)
    cap.record("upload_body_too_large", "POST", "/api/admin/packages", r.status_code,
               request={"file": "(zip，超上限)"}, response_body=r.json(),
               note="生成时临时将 max_upload_body_mb 置 0；真实默认 210MB")
    config.max_upload_body_mb = saved_limit
    r = client.post("/api/admin/packages", files={"file": ("pkg.zip", pkg_bytes, "application/zip")}, headers=ah)
    assert r.status_code == 200, r.text
    package_id = r.json()["id"]
    cap.record("upload_ok", "POST", "/api/admin/packages", r.status_code,
               request={"file": "(small_v1.zip)"}, response_body={"id": package_id})

    # --- 草稿预览与元数据修正 ---
    r = client.get("/api/admin/packages", headers=ah)
    cap.record("admin_packages_list", "GET", "/api/admin/packages", r.status_code, response_body=r.json())
    r = client.get(f"/api/admin/packages/{package_id}", headers=ah)
    assert r.status_code == 200
    preview = r.json()
    doc_hash = preview["documents"][0]["doc_hash"]
    cap.record("package_preview", "GET", f"/api/admin/packages/{package_id}", r.status_code, response_body=preview)
    r = client.get("/api/admin/packages/9999", headers=ah)
    cap.record("package_preview_missing", "GET", "/api/admin/packages/9999", r.status_code, response_body=r.json())
    r = client.patch(f"/api/admin/packages/{package_id}/documents/{doc_hash}",
                     json={"fields": {"title": "三下乡报名通知（修订标题）"}}, headers=ah)
    cap.record("patch_meta_ok", "PATCH", f"/api/admin/packages/{package_id}/documents/{doc_hash}", r.status_code,
               request={"fields": {"title": "…"}}, response_body=r.json())
    r = client.patch(f"/api/admin/packages/{package_id}/documents/{doc_hash}",
                     json={"fields": {"chunks": []}}, headers=ah)
    cap.record("patch_meta_forbidden_field", "PATCH", f"/api/admin/packages/{package_id}/documents/{doc_hash}",
               r.status_code, request={"fields": {"chunks": []}}, response_body=r.json())

    # --- 发布（错误替代目标 → 400） ---
    r = client.post(f"/api/admin/packages/{package_id}/publish", json={"replacements": {doc_hash: "nonexistent-uid"}},
                    headers=ah)
    cap.record("publish_bad_target", "POST", f"/api/admin/packages/{package_id}/publish", r.status_code,
               request={"replacements": {doc_hash: "nonexistent-uid"}}, response_body=r.json())
    r = client.post(f"/api/admin/packages/{package_id}/publish", json={"replacements": {}}, headers=ah)
    cap.record("publish_ok", "POST", f"/api/admin/packages/{package_id}/publish", r.status_code,
               request={"replacements": {}}, response_body=r.json())
    r = client.post(f"/api/admin/packages/{package_id}/publish", json={"replacements": {}}, headers=ah)
    cap.record("publish_twice", "POST", f"/api/admin/packages/{package_id}/publish", r.status_code,
               response_body=r.json())

    # --- 用户登录（访问码未设置 → 管理端设置 → 登录/旧码失效） ---
    r = client.get("/api/auth/state")
    cap.record("auth_state_set", "GET", "/api/auth/state", r.status_code, response_body=r.json())
    r = client.put("/api/admin/access-code", json={"code": "12"}, headers=ah)
    cap.record("access_code_too_short", "PUT", "/api/admin/access-code", r.status_code,
               request={"code": "12"}, response_body=r.json())
    r = client.put("/api/admin/access-code", json={"code": "2026fall"}, headers=ah)
    cap.record("access_code_set", "PUT", "/api/admin/access-code", r.status_code,
               request={"code": "2026fall"}, response_body=r.json())
    r = client.post("/api/auth/login", json={"code": "wrong-code"})
    cap.record("login_wrong_code", "POST", "/api/auth/login", r.status_code,
               request={"code": "wrong-code"}, response_body=r.json())
    r = client.post("/api/auth/login", json={"code": "2026fall"})
    assert r.status_code == 200
    user_token, client_id = r.json()["token"], r.json()["client_id"]
    uh = {"Authorization": f"Bearer {user_token}"}
    cap.record("login_ok", "POST", "/api/auth/login", r.status_code,
               request={"code": "2026fall"}, response_body={"token": "<jwt>", "client_id": client_id})
    r = client.put("/api/admin/access-code", json={"code": "2026winter"}, headers=ah)
    cap.record("access_code_rotate", "PUT", "/api/admin/access-code", r.status_code, response_body=r.json())
    r = client.post("/api/auth/login", json={"code": "2026fall"})
    cap.record("login_old_code_rejected", "POST", "/api/auth/login", r.status_code,
               request={"code": "2026fall"}, response_body=r.json())
    r = client.post("/api/auth/login", json={"code": "2026winter"})
    assert r.status_code == 200

    # --- 目录与原文 ---
    r = client.get("/api/catalog")
    cap.record("catalog_no_auth", "GET", "/api/catalog", r.status_code, response_body=r.json())
    r = client.get("/api/catalog", headers=uh)
    assert r.status_code == 200
    catalog = r.json()
    cap.record("catalog_ok", "GET", "/api/catalog", r.status_code, response_body=catalog)
    uid = next(d["doc_uid"] for d in catalog["documents"] if d["title"].startswith("校纪处分条例"))
    short_uid = next(d["doc_uid"] for d in catalog["documents"] if d["title"] == "三下乡报名通知（修订标题）")
    r = client.get(f"/api/source/{uid}/text?frm=1&to=250", headers=uh)
    assert r.status_code == 200
    body = r.json()
    cap.record("source_text_201_lines", "GET", f"/api/source/{uid}/text?frm=1&to=250", r.status_code,
               response_body={**body, "lines": body["lines"][:3] + ["…(共 %d 行)" % len(body["lines"])]},
               note=f"to 最多 frm+200：请求 to=250 实际返回 {len(body['lines'])} 行（闭区间含端点）")
    r = client.get(f"/api/source/{short_uid}/text?frm=0&to=999", headers=uh)
    assert r.status_code == 200
    cap.record("source_text_clamped", "GET", f"/api/source/{short_uid}/text?frm=0&to=999", r.status_code,
               response_body=r.json(), note="frm<1 取 1；to 超出行数取 line_count")
    r = client.get(f"/api/source/{short_uid}/text", headers=uh)
    cap.record("source_text_default_window", "GET", f"/api/source/{short_uid}/text", r.status_code,
               response_body=r.json(), note="默认 frm=1,to=80")
    r = client.get("/api/source/no-such-uid/text?frm=1&to=3", headers=uh)
    cap.record("source_text_missing", "GET", "/api/source/no-such-uid/text", r.status_code, response_body=r.json())
    r = client.get(f"/api/source/{short_uid}/text?frm=1&to=3")
    cap.record("source_text_no_auth", "GET", f"/api/source/{short_uid}/text", r.status_code, response_body=r.json())

    # --- 原文件下载（中文文件名 Content-Disposition） ---
    r = client.get(f"/api/source/{short_uid}/file", headers=uh)
    assert r.status_code == 200
    cap.record("source_file_ok", "GET", f"/api/source/{short_uid}/file", r.status_code,
               response_headers=sanitize_headers(r.headers), response_body="<原文件字节，doc_hash 命名>")
    r = client.get("/api/source/no-such-uid/file", headers=uh)
    cap.record("source_file_missing", "GET", "/api/source/no-such-uid/file", r.status_code, response_body=r.json())

    # --- 版本 ---
    r = client.get(f"/api/source/{short_uid}/versions", headers=uh)
    cap.record("source_versions", "GET", f"/api/source/{short_uid}/versions", r.status_code, response_body=r.json())
    r = client.get(f"/api/admin/documents/{short_uid}/versions", headers=ah)
    cap.record("admin_versions", "GET", f"/api/admin/documents/{short_uid}/versions", r.status_code,
               response_body=r.json())
    r = client.get(f"/api/admin/documents/{short_uid}/versions", headers=uh)
    cap.record("admin_versions_wrong_role", "GET", f"/api/admin/documents/{short_uid}/versions", r.status_code,
               response_body=r.json(), note="用户令牌访问管理端 → 401")

    # --- 管理端文档与状态 ---
    r = client.get("/api/admin/documents", headers=ah)
    cap.record("admin_documents", "GET", "/api/admin/documents", r.status_code, response_body=r.json())
    r = client.get("/api/admin/status", headers=ah)
    cap.record("admin_status", "GET", "/api/admin/status", r.status_code, response_body=r.json())
    r = client.get("/api/admin/metrics", headers=ah)
    m = r.json()
    cap.record("admin_metrics", "GET", "/api/admin/metrics", r.status_code, response_body=m,
               note="rss_bytes/cpu_s/disk_free_bytes 为动态值；字段名与类型是契约")

    # --- 停用/启用/解除替代 ---
    r = client.post(f"/api/admin/documents/{short_uid}/deactivate", headers=ah)
    cap.record("deactivate_ok", "POST", f"/api/admin/documents/{short_uid}/deactivate", r.status_code,
               response_body=r.json())
    r = client.get("/api/catalog", headers=uh)
    cap.record("catalog_after_deactivate", "GET", "/api/catalog", r.status_code,
               response_body={**r.json(), "documents": "<少一份，已停用资料不再出现>"})
    r = client.post(f"/api/admin/documents/{short_uid}/deactivate", headers=ah)
    cap.record("deactivate_twice", "POST", f"/api/admin/documents/{short_uid}/deactivate", r.status_code,
               response_body=r.json())
    r = client.post(f"/api/admin/documents/{short_uid}/enable", headers=ah)
    cap.record("enable_ok", "POST", f"/api/admin/documents/{short_uid}/enable", r.status_code, response_body=r.json())
    r = client.post("/api/admin/documents/no-such-uid/enable", headers=ah)
    cap.record("enable_missing", "POST", "/api/admin/documents/no-such-uid/enable", r.status_code,
               response_body=r.json())
    r = client.post(f"/api/admin/documents/{short_uid}/unlink", headers=ah)
    cap.record("unlink_no_relation", "POST", f"/api/admin/documents/{short_uid}/unlink", r.status_code,
               response_body=r.json())

    # --- 聊天非流式错误分支（有效请求会直接进入 SSE 流，快照见 sse/） ---
    r = client.post("/api/chat", json={"question": ""}, headers=uh)
    cap.record("chat_question_empty", "POST", "/api/chat", r.status_code,
               request={"question": ""}, response_body=r.json())
    r = client.post("/api/chat", json={"question": "hi", "scope": {"mode": "files", "doc_uids": []}}, headers=uh)
    cap.record("chat_files_empty", "POST", "/api/chat", r.status_code,
               request={"scope": {"mode": "files", "doc_uids": []}}, response_body=r.json())
    r = client.post("/api/chat", json={"question": "x" * 4001}, headers=uh)
    cap.record("chat_question_too_long", "POST", "/api/chat", r.status_code,
               request={"question": "(4001 字符)"}, response_body=r.json())
    r = client.post("/api/chat", json={"question": "hi"})
    cap.record("chat_no_auth", "POST", "/api/chat", r.status_code, response_body=r.json())
    r = client.post("/api/chat/cancel", json={"request_id": "unknown"}, headers=uh)
    cap.record("chat_cancel_unknown", "POST", "/api/chat/cancel", r.status_code,
               request={"request_id": "unknown"}, response_body=r.json())
    r = client.post("/api/chat/cancel", json={"request_id": "x"})
    cap.record("chat_cancel_no_auth", "POST", "/api/chat/cancel", r.status_code, response_body=r.json())

    # --- 备份与恢复 ---
    r = client.get("/api/admin/backup", headers=ah)
    assert r.status_code == 200
    backup_bytes = r.content
    cap.record("backup_download", "GET", "/api/admin/backup", r.status_code,
               response_headers=sanitize_headers(r.headers), response_body="<zip 字节，PK 开头>")
    r = client.post("/api/admin/restore", files={"file": ("broken.zip", b"not a zip", "application/zip")}, headers=ah)
    cap.record("restore_broken", "POST", "/api/admin/restore", r.status_code,
               request={"file": "(非法 zip)"}, response_body=r.json())
    r = client.post("/api/admin/restore", files={"file": ("backup.zip", backup_bytes, "application/zip")}, headers=ah)
    cap.record("restore_ok", "POST", "/api/admin/restore", r.status_code,
               request={"file": "(backup.zip)"}, response_body=r.json())

    # --- 草稿丢弃 ---
    up = client.post("/api/admin/packages", files={"file": ("pkg2.zip", pkg_bytes, "application/zip")}, headers=ah)
    # 相同包哈希 → 400 重复导入
    cap.record("upload_duplicate", "POST", "/api/admin/packages", up.status_code,
               request={"file": "(与已导入包字节一致)"}, response_body=up.json())
    pkg3 = client.post("/api/admin/packages",
                       files={"file": ("pkg3.zip", mutate_package(pkg_bytes), "application/zip")}, headers=ah)
    if pkg3.status_code == 200:
        pid3 = pkg3.json()["id"]
        r = client.delete(f"/api/admin/packages/{pid3}", headers=ah)
        cap.record("discard_draft", "DELETE", f"/api/admin/packages/{pid3}", r.status_code, response_body=r.json())
        r = client.delete(f"/api/admin/packages/{pid3}", headers=ah)
        cap.record("discard_twice", "DELETE", f"/api/admin/packages/{pid3}", r.status_code, response_body=r.json())

    # --- 登录限流 429（放最后，避免污染其他用例的桶） ---
    limiter._buckets.clear()
    statuses = []
    for _ in range(11):
        statuses.append(client.post("/api/auth/admin/login", json={"password": "nope"}).status_code)
    cap.record("admin_login_rate_limited", "POST", "/api/auth/admin/login", statuses[-1],
               request={"password": "nope", "attempts": 11}, response_body=client.post(
                   "/api/auth/admin/login", json={"password": "nope"}).json(),
               note=f"11 连发状态序列 {statuses}；2/min、burst 10")


def mutate_package(pkg_bytes: bytes) -> bytes:
    """改动一个字节得到不同包哈希的合法包（用于丢弃草稿用例）。"""
    src = io.BytesIO(pkg_bytes)
    out = io.BytesIO()
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zout:
        for info in zin.infolist():
            data = zin.read(info.filename)
            if info.filename == "manifest.json":
                manifest = json.loads(data)
                manifest["documents"][0]["notes"] = "variant"
                data = json.dumps(manifest, ensure_ascii=False).encode()
            zout.writestr(info.filename, data)
    return out.getvalue()


# ---------- 6. SSE 事件快照 ----------

class FakeGeminiScripted:
    """按脚本回放：functionCall（含 id）、带 thoughtSignature 的 part、空文本签名 part。"""

    def __init__(self, script):
        self.script = script
        self.calls = 0

    async def stream(self, contents, system=None, tools=None, temperature=0.2):
        has_tool_result = any(
            "functionResponse" in p for m in contents if m["role"] == "user" for p in m["parts"]
        )
        step = self.script[min(self.calls, len(self.script) - 1)]
        self.calls += 1
        for item in step:
            if item.get("sleep"):
                await asyncio.sleep(item["sleep"])
                continue
            if "parts" in item:
                if has_tool_result or item.get("force_parts"):
                    for p in item["parts"]:
                        if "text" in p and not p.get("thought"):
                            yield {"type": "text", "text": p["text"]}
                    yield {"type": "parts", "parts": item["parts"]}
                continue
            if "text" in item and (has_tool_result or not tools):
                yield {"type": "text", "text": item["text"]}
        yield {"type": "parts", "parts": _flatten(self.script[min(self.calls - 1, len(self.script) - 1)])}

    async def close(self):
        return None


def _flatten(step):
    parts = []
    for item in step:
        if "parts" in item:
            parts.extend(item["parts"])
        elif "text" in item:
            parts.append({"text": item["text"]})
    return parts


async def fake_embed_unit(text, dims=None):
    v = np.zeros(1024, dtype=np.float32)
    v[0] = 1.0
    return v


def capture_chat_events(client, uh, client_id, body):
    events = []
    with client.stream("POST", "/api/chat", json=body, headers={**uh, "X-Client-Id": client_id}) as resp:
        assert resp.status_code == 200, resp.text
        for line in resp.iter_lines():
            if line.startswith("data: "):
                events.append(json.loads(line[6:]))
    return events


def drive_sse(client, ah, uh, client_id):
    from app import api as api_mod

    scenarios = {}

    # 场景 1：完整成功流（工具调用 + 签名 part + 伪造引用剔除）
    scripted = FakeGeminiScripted([
        [
            {"parts": [{"text": "（内部检索计划，不外发）", "thought": True, "thoughtSignature": "SIG-THOUGHT-1"},
                        {"functionCall": {"name": "policy_search", "args": {"query": "三下乡 报名"}, "id": "call-1"}}]},
        ],
        [
            {"parts": [{"text": "", "thoughtSignature": "SIG-EMPTY-TEXT"},
                        {"text": "报名需要在6月22日前完成 [[EV1]]，伪造的 [[EV99]] 应被剔除。"}]},
        ],
    ])
    api_mod.agent.gemini = scripted
    api_mod.agent._embed = fake_embed_unit  # type: ignore[attr-defined]
    import app.agent as agent_mod

    orig_embed = agent_mod.embed_query
    agent_mod.embed_query = fake_embed_unit
    body = {"question": "三下乡什么时候报名？", "messages": [], "scope": {"mode": "auto"},
            "profile": {"college": "信息工程学院", "entry_year": "2025"}}
    scenarios["full_flow"] = capture_chat_events(client, uh, client_id, body)
    agent_mod.embed_query = orig_embed

    # 场景 2：expand_request（不发 done）
    scripted2 = FakeGeminiScripted([
        [{"parts": [{"text": "范围不足，[[EXPAND_REQUEST:需要检索其他学院的相关规定]]"}], "force_parts": True}],
    ])
    api_mod.agent.gemini = scripted2
    body2 = {"question": "其他学院的规定是什么？", "messages": [], "scope": {"mode": "domains",
            "domains": ["安全纪律"]}, "profile": {}}
    scenarios["expand_request"] = capture_chat_events(client, uh, client_id, body2)

    # 场景 3：工具失败（read_source 不存在的资料 → stage failed）
    scripted3 = FakeGeminiScripted([
        [{"parts": [{"functionCall": {"name": "read_source",
                                        "args": {"doc_uid": "no-such-doc", "line_start": 1, "line_end": 5},
                                        "id": "call-2"}}]}],
        [{"parts": [{"text": "未能读取该资料原文，无法基于资料回答。"}]}],
    ])
    api_mod.agent.gemini = scripted3
    scenarios["tool_failure"] = capture_chat_events(client, uh, client_id,
                                                    {"question": "读一下资料", "messages": [], "scope": {}, "profile": {}})

    # 场景 4：错误响应（上游非 2xx → SSE error 事件）
    class FailingGemini:
        async def stream(self, *a, **k):
            from app.llm import LLMError

            raise LLMError("模型调用失败 HTTP 500")
            yield  # noqa: unreachable——使本函数成为异步生成器，进入 Agent 的 LLMError 分支

        async def close(self):
            return None

    api_mod.agent.gemini = FailingGemini()
    scenarios["upstream_error"] = capture_chat_events(client, uh, client_id,
                                                      {"question": "触发上游错误", "messages": [], "scope": {}, "profile": {}})

    # 场景 5：排队 → started（两个不同 client_id 的用户；第二个应先收到 queued）
    class SlowGemini:
        async def stream(self, *a, **k):
            yield {"type": "text", "text": "慢"}
            await asyncio.sleep(0.4)
            yield {"type": "parts", "parts": [{"text": "慢"}]}

        async def close(self):
            return None

    api_mod.agent.gemini = SlowGemini()
    token1, cid1 = _user_login(client)
    token2, cid2 = _user_login(client)
    events_first, events_second = [], []

    async def two_clients():
        import httpx

        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=client.app), base_url="http://testserver") as hc:
            async def stream_one(tok, cid, sink):
                async with hc.stream("POST", "/api/chat",
                                     json={"question": "排队测试", "messages": [], "scope": {}, "profile": {}},
                                     headers={"Authorization": f"Bearer {tok}", "X-Client-Id": cid}) as resp:
                    async for line in resp.aiter_lines():
                        if line.startswith("data: "):
                            sink.append(json.loads(line[6:]))

            t1 = asyncio.create_task(stream_one(token1, cid1, events_first))
            await asyncio.sleep(0.05)
            t2 = asyncio.create_task(stream_one(token2, cid2, events_second))
            await asyncio.gather(t1, t2)

    asyncio.run(two_clients())
    return {
        "full_flow": {"question": "三下乡什么时候报名？", "profile": {"college": "信息工程学院", "entry_year": "2025"},
                      "events": scenarios["full_flow"]},
        "expand_request": {"question": "其他学院的规定是什么？", "scope": {"mode": "domains", "domains": ["安全纪律"]},
                           "events": scenarios["expand_request"]},
        "tool_failure": {"question": "读一下资料", "events": scenarios["tool_failure"]},
        "upstream_error": {"question": "触发上游错误", "events": scenarios["upstream_error"]},
        "queued_second_client": {
            "first_client_events": events_first,
            "second_client_events": events_second,
            "note": "两个不同 client_id 的用户并发提问；第二个应先 queued(position=1) 后 started",
        },
    }


# ---------- main ----------

def main():
    _freeze_time()
    if FIXTURES.exists():
        import shutil

        shutil.rmtree(FIXTURES)
    FIXTURES.mkdir(parents=True)

    gen_token_vectors()
    gen_password_vectors()
    gen_jieba_golden()
    gen_fts_bm25_golden()
    gen_vector_golden()
    gen_config_defaults()

    pkg_bytes, manifest = build_small_package()
    write_bytes("packages/small_v1.zip", pkg_bytes)
    write_json("packages/manifest_snapshot.json", manifest)

    # 应用栈：手动注入（不经 lifespan，避免真实目录）
    from fastapi.testclient import TestClient
    from app.agent import Agent
    from app.chat import ChatManager
    from app.db import Database
    from app.main import app
    from app.security import Tokens, init_admin
    from app.vectors import get_index

    db = Database(config.data_dir / "campus.db")
    init_admin(db)
    app_api.db = db
    app_api.tokens = Tokens(db)
    app_api.chats = ChatManager()
    agent = Agent(db, get_index(config.data_dir / "vectors"))
    app_api.agent = agent
    client = TestClient(app)

    drive_routes(client, pkg_bytes)
    limiter._buckets.clear()

    # SSE 用的账号
    ah = {"Authorization": f"Bearer " + _admin_login(client)}
    client.put("/api/admin/access-code", json={"code": "2026fall"}, headers=ah)
    r = client.post("/api/auth/login", json={"code": "2026fall"})
    uh = {"Authorization": f"Bearer {r.json()['token']}"}
    client_id = r.json()["client_id"]
    scenarios = drive_sse(client, ah, uh, client_id)
    for name, payload in scenarios.items():
        write_json(f"sse/{name}.json", payload)

    agent.vectors.close_all()
    db.close()
    write_json("meta.json", {
        "generated_by": "scripts/rust/gen_fixtures.py",
        "python": sys.version.split()[0],
        "jieba_version": importlib.metadata.version("jieba") if importlib.metadata else None,
        "numpy_version": np.__version__,
        "sqlite_version": sqlite3.sqlite_version,
        "fastapi": importlib.metadata.version("fastapi") if importlib.metadata else None,
        "fixed_utcnow": FIXED_NOW,
        "note": "快照类夹具在生成时刻取值；重跑会得到不同随机 id，以 git 提交版本为准",
    })
    print(f"OK: {_write_count} fixture files -> {FIXTURES}")


def _admin_login(client) -> str:
    r = client.post("/api/auth/admin/login", json={"password": "admin1234"})
    assert r.status_code == 200, r.text
    return r.json()["admin_token"]


def _user_login(client) -> tuple[str, str]:
    r = client.post("/api/auth/login", json={"code": "2026fall"})
    assert r.status_code == 200, r.text
    return r.json()["token"], r.json()["client_id"]


if __name__ == "__main__":
    main()
