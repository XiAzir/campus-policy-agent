"""资料包导入、预览、元数据修正、发布、两类停用与版本管理。

导入 → 草稿（全量校验落盘）→ 管理员预览/修正 → 事务性发布（新版就绪才切换现行版本）。
发布时确认替代关系：旧版自动进入替代停用；已手动停用的旧版保持手动停用。
"""

from __future__ import annotations

import json
import re
import shutil
import uuid
from pathlib import Path

from .config import config
from .db import Database, utcnow
from .pkgfmt import PackageError, validate_package

EDITABLE_FIELDS = {"title", "department", "effective_date", "audience", "notes", "replaces"}


class IngestError(Exception):
    pass


def package_path(package_sha: str) -> Path:
    return config.data_dir / "packages" / f"{package_sha}.zip"


def import_package(db: Database, data: bytes, original_filename: str) -> int:
    """校验并落盘为草稿；返回 package id。重复导入（包哈希一致）拒绝。"""
    try:
        manifest, docs, vecs, files, texts = validate_package(data)
    except PackageError as exc:
        raise IngestError(f"校验未通过：{exc}") from exc

    import hashlib

    package_sha = hashlib.sha256(data).hexdigest()
    if db.one("SELECT id FROM packages WHERE sha256=?", (package_sha,)):
        raise IngestError("该资料包已导入过（包哈希一致），拒绝重复导入")

    dest = package_path(package_sha)
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(".tmp")
    tmp.write_bytes(data)
    tmp.replace(dest)

    vectors_dir = config.data_dir / "vectors"
    vectors_dir.mkdir(parents=True, exist_ok=True)
    import numpy as np

    from .vectors import close_mmap_for

    draft_vec = vectors_dir / f"pkg-draft-{package_sha[:16]}.npy"
    close_mmap_for(vectors_dir, draft_vec)
    np.save(draft_vec, vecs)

    with db.tx() as conn:
        cur = conn.execute(
            "INSERT INTO packages(sha256, original_filename, size, imported_at, status,"
            " preprocessing_version, embed_model, embed_dim, doc_count, chunk_count)"
            " VALUES(?,?,?,?, 'draft', ?,?,?,?,?)",
            (
                package_sha,
                original_filename,
                len(data),
                utcnow(),
                manifest["preprocessing_version"],
                manifest["embed_model"],
                manifest["embed_dim"],
                len(docs),
                manifest["vectors"]["count"],
            ),
        )
        package_id = cur.lastrowid
    db.audit("admin", "package_import", f"package={package_id} docs={len(docs)} chunks={manifest['vectors']['count']}")
    return package_id


def _overrides(db: Database, package_id: int) -> dict:
    row = db.one("SELECT meta_overrides FROM packages WHERE id=?", (package_id,))
    return json.loads(row["meta_overrides"]) if row else {}


def apply_override(db: Database, package_id: int, doc_hash: str, fields: dict) -> None:
    """草稿阶段元数据修正：仅允许不影响分块/向量的字段。"""
    bad = set(fields) - EDITABLE_FIELDS
    if bad:
        raise IngestError(
            f"字段 {sorted(bad)} 会影响分块或向量，修改后必须重新生成资料包（本地预处理）"
        )
    row = db.one("SELECT status FROM packages WHERE id=?", (package_id,))
    if row is None:
        raise IngestError("资料包不存在")
    if row["status"] != "draft":
        raise IngestError("只有草稿状态可以修正元数据")
    ov = _overrides(db, package_id)
    ov.setdefault(doc_hash, {}).update(fields)
    with db.tx() as conn:
        conn.execute(
            "UPDATE packages SET meta_overrides=? WHERE id=?", (json.dumps(ov, ensure_ascii=False), package_id)
        )
    db.audit("admin", "package_meta_edit", f"package={package_id} doc={doc_hash[:12]}… fields={sorted(fields)}")


def preview_package(db: Database, package_id: int) -> dict | None:
    """草稿预览：包信息、校验结论、逐文档元数据（含 overrides 与替代目标提示）。"""
    pkg = db.one("SELECT * FROM packages WHERE id=?", (package_id,))
    if pkg is None:
        return None
    import zipfile

    import numpy as np

    from .pkgfmt import sha256_bytes

    data = package_path(pkg["sha256"]).read_bytes()
    manifest, docs, vecs, files, texts = validate_package(data)  # 预览时复核
    ov = _overrides(db, package_id)
    doc_list = []
    for d in docs:
        meta = ov.get(d["doc_hash"], {})
        target = d.get("replaces") or meta.get("replaces")
        target_uid = None
        if target:
            # 允许填旧版 doc_hash 或 doc_uid
            row = db.one(
                "SELECT doc_uid FROM documents WHERE doc_hash=? OR doc_uid=?", (target, target)
            )
            target_uid = row["doc_uid"] if row else None
        doc_list.append(
            {
                "doc_hash": d["doc_hash"],
                "original_filename": d["original_filename"],
                "doc_type": d["doc_type"],
                "title": meta.get("title", d["title"]),
                "department": meta.get("department", d["department"]),
                "effective_date": meta.get("effective_date", d["effective_date"]),
                "audience": meta.get("audience", d["audience"]),
                "notes": meta.get("notes", d.get("notes", "")),
                "replaces_doc_uid": target_uid,
                "replaces_unresolved": bool(target) and target_uid is None,
                "domains": d.get("domains", []),
                "sections": d.get("sections", []),
                "line_count": d["line_count"],
                "chunk_count": len(d["chunks"]),
                "edited": bool(ov.get(d["doc_hash"])),
            }
        )
    return {
        "id": pkg["id"],
        "sha256": pkg["sha256"],
        "original_filename": pkg["original_filename"],
        "size": pkg["size"],
        "imported_at": pkg["imported_at"],
        "status": pkg["status"],
        "preprocessing_version": pkg["preprocessing_version"],
        "embed_model": pkg["embed_model"],
        "embed_dim": pkg["embed_dim"],
        "doc_count": pkg["doc_count"],
        "chunk_count": pkg["chunk_count"],
        "vector_shape": list(vecs.shape) if vecs is not None else None,
        "documents": doc_list,
    }


def publish_package(db: Database, package_id: int, replacements: dict[str, str | None]) -> None:
    """事务性发布。replacements: doc_hash → 被替代旧版的 doc_uid（或 null/缺省=不替代）。"""
    pkg = db.one("SELECT * FROM packages WHERE id=?", (package_id,))
    if pkg is None:
        raise IngestError("资料包不存在")
    if pkg["status"] != "draft":
        raise IngestError("资料包不是草稿状态")
    data = package_path(pkg["sha256"]).read_bytes()
    manifest, docs, vecs, files, texts = validate_package(data)
    ov = _overrides(db, package_id)

    # 向量文件就位（发布即从草稿名改为正式名）
    vectors_dir = config.data_dir / "vectors"
    draft_vec = vectors_dir / f"pkg-draft-{pkg['sha256'][:16]}.npy"
    final_vec = vectors_dir / f"pkg-{package_id}.npy"
    if not draft_vec.exists():
        raise IngestError("向量文件缺失，导入不完整")
    from .vectors import close_mmap_for

    close_mmap_for(vectors_dir, final_vec)
    close_mmap_for(vectors_dir, draft_vec)
    shutil.move(str(draft_vec), str(final_vec))

    files_dir = config.data_dir / "files"
    text_dir = config.data_dir / "text"
    files_dir.mkdir(parents=True, exist_ok=True)
    text_dir.mkdir(parents=True, exist_ok=True)
    try:
        for fname, blob in files.items():
            dest = files_dir / fname.replace("files/", "", 1)
            if not dest.exists():
                dest.write_bytes(blob)
        for tname, text in texts.items():
            dest = text_dir / tname.replace("text/", "", 1)
            if not dest.exists():
                dest.write_text(text, encoding="utf-8")
    except OSError as exc:
        raise IngestError(f"落盘失败，发布中止：{exc}") from exc

    with db.tx() as conn:
        for d in docs:
            h = d["doc_hash"]
            meta = ov.get(h, {})
            ext = Path(d["original_filename"]).suffix.lower()
            new_uid = uuid.uuid4().hex
            target = (meta.get("replaces") if "replaces" in meta else d.get("replaces")) or None
            target = replacements.get(h, target)
            replaces_id = None
            if target:
                old = conn.execute(
                    "SELECT id, deactivated_kind FROM documents WHERE doc_uid=? OR doc_hash=?",
                    (target, target),
                ).fetchone()
                if old is None:
                    raise IngestError(f"替代目标不存在：{target}")
                replaces_id = old["id"]
                # 手动停用优先于替代停用：旧版已手动停用时保持 manual
                if old["deactivated_kind"] == "":
                    conn.execute(
                        "UPDATE documents SET deactivated_kind='superseded', deactivated_at=? WHERE id=?",
                        (utcnow(), old["id"]),
                    )
            cur = conn.execute(
                "INSERT INTO documents(doc_uid, doc_hash, package_id, title, original_filename,"
                " doc_type, department, effective_date, audience, notes, line_count, page_map,"
                " text_sha256, replaces_doc_id, published_at)"
                " VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    new_uid,
                    h,
                    package_id,
                    meta.get("title", d["title"]),
                    d["original_filename"],
                    d["doc_type"],
                    meta.get("department", d["department"]),
                    meta.get("effective_date", d["effective_date"]),
                    json.dumps(meta.get("audience", d["audience"]), ensure_ascii=False),
                    meta.get("notes", d.get("notes", "")),
                    d["line_count"],
                    json.dumps(d.get("page_map")) if d.get("page_map") else None,
                    d["text_sha256"],
                    replaces_id,
                    utcnow(),
                ),
            )
            doc_id = cur.lastrowid
            for sec in d.get("sections", []):
                conn.execute(
                    "INSERT INTO sections(doc_id, section_id, title, start_line, end_line) VALUES(?,?,?,?,?)",
                    (doc_id, sec["section_id"], sec["title"], sec["start_line"], sec["end_line"]),
                )
            for dom in d.get("domains", []):
                sids = dom.get("section_ids") or [None]
                for sid in sids:
                    conn.execute(
                        "INSERT INTO doc_tags(doc_id, section_id, tag) VALUES(?,?,?)",
                        (doc_id, sid if sid else None, dom["tag"]),
                    )
            for ch in d["chunks"]:
                ccur = conn.execute(
                    "INSERT INTO chunks(doc_id, chunk_index, text, line_start, line_end, section_id)"
                    " VALUES(?,?,?,?,?,?)",
                    (doc_id, ch["vector_index"], ch["text"], ch["line_start"], ch["line_end"], ch.get("section_id")),
                )
                chunk_id = ccur.lastrowid
                from .db import Database as _DB  # noqa: F401  (fts_insert 为实例方法)

                conn.execute(
                    "INSERT INTO chunks_fts(rowid, body) VALUES(?,?)",
                    (chunk_id, _fts_body(ch["text"])),
                )
                conn.execute(
                    "INSERT INTO vector_rows(chunk_id, row_index, doc_id, package_id) VALUES(?,?,?,?)",
                    (chunk_id, ch["vector_index"], doc_id, package_id),
                )
        conn.execute("UPDATE packages SET status='published' WHERE id=?", (package_id,))
    db.audit("admin", "package_publish", f"package={package_id} docs={len(docs)}")


def _fts_body(text: str) -> str:
    from .textutil import tokenize_for_fts

    return tokenize_for_fts(text)


def deactivate_document(db: Database, doc_uid: str) -> None:
    row = db.one("SELECT id, deactivated_kind FROM documents WHERE doc_uid=?", (doc_uid,))
    if row is None:
        raise IngestError("资料不存在")
    if row["deactivated_kind"] == "manual":
        raise IngestError("该资料已处于手动停用状态")
    with db.tx() as conn:
        conn.execute(
            "UPDATE documents SET deactivated_kind='manual', deactivated_at=? WHERE id=?",
            (utcnow(), row["id"]),
        )
    db.audit("admin", "document_deactivate", f"doc={doc_uid}")


def enable_document(db: Database, doc_uid: str) -> None:
    """重新启用。已被替代的旧版须先解除替代关系，避免两个现行版本并存。"""
    row = db.one("SELECT id, deactivated_kind FROM documents WHERE doc_uid=?", (doc_uid,))
    if row is None:
        raise IngestError("资料不存在")
    if row["deactivated_kind"] == "":
        return
    if row["deactivated_kind"] == "superseded":
        newer = db.one(
            "SELECT doc_uid, title FROM documents WHERE replaces_doc_id=?", (row["id"],)
        )
        if newer:
            raise IngestError(
                f"该版本已被《{newer['title']}》（{newer['doc_uid'][:8]}…）替代，须先解除替代关系再启用"
            )
    with db.tx() as conn:
        conn.execute(
            "UPDATE documents SET deactivated_kind='', deactivated_at=NULL WHERE id=?", (row["id"],)
        )
    db.audit("admin", "document_enable", f"doc={doc_uid}")


def unlink_replacement(db: Database, doc_uid: str) -> None:
    """解除替代关系：新版不再指向旧版；旧版保持替代停用状态，可再手动启用。"""
    row = db.one("SELECT id, replaces_doc_id FROM documents WHERE doc_uid=?", (doc_uid,))
    if row is None:
        raise IngestError("资料不存在")
    if row["replaces_doc_id"] is None:
        raise IngestError("该资料没有替代关系")
    old = db.one("SELECT doc_uid FROM documents WHERE id=?", (row["replaces_doc_id"],))
    with db.tx() as conn:
        conn.execute("UPDATE documents SET replaces_doc_id=NULL WHERE id=?", (row["id"],))
    db.audit("admin", "document_unlink", f"doc={doc_uid} unlink_from={old['doc_uid'] if old else '?'}")


def version_history(db: Database, doc_uid: str) -> list[dict]:
    """版本链：从任一版本出发，向前（被其替代）与向后（替代它）遍历。"""
    row = db.one("SELECT * FROM documents WHERE doc_uid=?", (doc_uid,))
    if row is None:
        return []
    chain = [row]
    back = row
    while back["replaces_doc_id"]:
        back = db.one("SELECT * FROM documents WHERE id=?", (back["replaces_doc_id"],))
        if back is None:
            break
        chain.insert(0, back)
    fwd = row
    while True:
        fwd = db.one("SELECT * FROM documents WHERE replaces_doc_id=?", (fwd["id"],))
        if fwd is None:
            break
        chain.append(fwd)
    return [
        {
            "doc_uid": r["doc_uid"],
            "title": r["title"],
            "published_at": r["published_at"],
            "effective_date": r["effective_date"],
            "deactivated_kind": r["deactivated_kind"],
            "is_current": r["deactivated_kind"] == "",
            "package_id": r["package_id"],
        }
        for r in chain
    ]
