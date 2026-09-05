"""混合检索：SQLite FTS5（jieba）+ 向量点积，RRF 排名融合。

版本过滤规则（plan 第二节/第三节）：
- 默认（现行模式）：deactivated_kind='' 的文档。
- 往年模式：额外纳入 deactivated_kind='superseded' 的旧版。
- 手动停用（manual）任何模式都不可检索。
- 领域标签、明确文件限制在此基础上进一步过滤。
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import numpy as np

from .db import Database
from .textutil import fts_match_query
from .vectors import VectorIndex

RRF_K = 60
TOP_K_FTS_FACTOR = 3


def allowed_doc_ids(
    db: Database,
    *,
    year_mode: str = "current",
    domains: list[str] | None = None,
    doc_uids: list[str] | None = None,
) -> set[int]:
    """按可见性（版本模式）、领域标签、明确文件限制过滤，返回 doc_id 集合。"""
    kinds = ("",) if year_mode != "past" else ("", "superseded")
    marks = ",".join("?" for _ in kinds)
    rows = db.q(
        f"SELECT id, doc_uid FROM documents WHERE deactivated_kind IN ({marks})", kinds
    )
    id_by_uid = {r["doc_uid"]: r["id"] for r in rows}
    if doc_uids:
        return {id_by_uid[u] for u in doc_uids if u in id_by_uid}
    ids = {r["id"] for r in rows}
    if domains:
        marks = ",".join("?" for _ in domains)
        tag_rows = db.q(
            f"SELECT DISTINCT doc_id FROM doc_tags WHERE tag IN ({marks})", tuple(domains)
        )
        ids &= {r["doc_id"] for r in tag_rows}
    return ids


def page_for_line(page_map: list[list[int]], line: int) -> int | None:
    """按 page_map（[标记行, 页码] 区段升序）求行号对应 PDF 页码。"""
    if not page_map:
        return None
    page = None
    for mark_line, pg in page_map:
        if mark_line <= line:
            page = pg
        else:
            break
    return page


def read_lines(db: Database, doc_row: sqlite3.Row, line_start: int, line_end: int) -> list[str]:
    """从磁盘标准化原文读取行（历史引用打开也走这里，不依赖聊天记录）。"""
    path = Path(doc_row["_text_path"]) if "_text_path" in doc_row.keys() else None
    if path is None:
        from .config import config as cfg

        path = cfg.data_dir / "text" / f"{doc_row['doc_hash']}.txt"
    lines = path.read_text(encoding="utf-8").splitlines()
    return lines[max(0, line_start - 1) : max(0, line_end)]


def search(
    db: Database,
    vectors: VectorIndex,
    query: str,
    query_vec: np.ndarray | None,
    scope_doc_ids: set[int],
    top_k: int = 8,
) -> list[dict]:
    """返回融合排序的证据列表（含文档与章节信息）。query 为空或范围为空返回空。"""
    if not query.strip() or not scope_doc_ids:
        return []
    id_marks = ",".join("?" for _ in scope_doc_ids)
    params: list = list(scope_doc_ids)

    fts_hits: list[tuple[int, float]] = []  # (chunk_id, rrf 前原始分)
    match = fts_match_query(query)
    if match:
        try:
            rows = db.q(
                f"SELECT f.rowid AS chunk_id, bm25(chunks_fts) AS score "
                f"FROM chunks_fts f JOIN chunks c ON c.id=f.rowid "
                f"WHERE chunks_fts MATCH ? AND c.doc_id IN ({id_marks}) "
                f"ORDER BY score LIMIT ?",
                (match, *params, top_k * TOP_K_FTS_FACTOR),
            )
            fts_hits = [(r["chunk_id"], -float(r["score"])) for r in rows]
        except sqlite3.OperationalError:
            fts_hits = []

    vec_hits: list[tuple[int, float]] = []
    if query_vec is not None:
        vrows = db.q(
            f"SELECT package_id, row_index, chunk_id FROM vector_rows WHERE doc_id IN ({id_marks})",
            params,
        )
        candidates: dict[int, list[int]] = {}
        by_row: dict[tuple[int, int], int] = {}
        for r in vrows:
            candidates.setdefault(r["package_id"], []).append(r["row_index"])
            by_row[(r["package_id"], r["row_index"])] = r["chunk_id"]
        for pkg, row in vectors.search(query_vec, candidates, top_k * TOP_K_FTS_FACTOR):
            cid = by_row.get((pkg, row))
            if cid is not None:
                vec_hits.append((cid, 1.0))

    scores: dict[int, float] = {}
    for rank, (cid, _) in enumerate(fts_hits):
        scores[cid] = scores.get(cid, 0) + 1.0 / (RRF_K + rank + 1)
    for rank, (cid, _) in enumerate(vec_hits):
        scores[cid] = scores.get(cid, 0) + 1.0 / (RRF_K + rank + 1)
    ordered = sorted(scores.items(), key=lambda kv: -kv[1])[:top_k]
    if not ordered:
        return []

    chunk_marks = ",".join("?" for _ in ordered)
    info = {
        r["id"]: r
        for r in db.q(
            f"SELECT * FROM chunks WHERE id IN ({chunk_marks})", tuple(cid for cid, _ in ordered)
        )
    }
    docs = {r["id"]: r for r in db.q(f"SELECT * FROM documents WHERE id IN ({id_marks})", params)}

    results: list[dict] = []
    for cid, score in ordered:
        ch = info.get(cid)
        if ch is None:
            continue
        doc = docs.get(ch["doc_id"])
        if doc is None:
            continue
        results.append(
            {
                "chunk_id": cid,
                "score": round(score, 6),
                "doc_uid": doc["doc_uid"],
                "doc_hash": doc["doc_hash"],
                "title": doc["title"],
                "doc_type": doc["doc_type"],
                "line_start": ch["line_start"],
                "line_end": ch["line_end"],
                "section_id": ch["section_id"],
                "page": page_for_line(json.loads(doc["page_map"]) if doc["page_map"] else [], ch["line_start"]),
                "text": ch["text"],
            }
        )
    return results
