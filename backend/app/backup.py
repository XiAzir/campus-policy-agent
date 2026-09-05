"""Snapshot backups with bounded validation and same-filesystem rollback."""

from contextlib import closing, contextmanager
import io
import json
import os
import re
import shutil
import sqlite3
import tempfile
import uuid
import zipfile
from pathlib import Path, PurePosixPath

import numpy as np

from .config import config
from .db import utcnow
from .storage import BLOCK_BYTES, hash_file, remove_tree, require_space

BACKUP_FORMAT = "campus-policy-backup"
BACKUP_VERSION = 2
SUBDIRS = ("files", "text", "vectors", "packages")
MAX_EXPANDED = 10 * 1024**3
MAX_ENTRIES = 100_000
MAX_META = 16 * 1024**2
HASH = re.compile(r"^[a-f0-9]{64}$")


def _snapshot_db(db_path, dest):
    with closing(sqlite3.connect(str(db_path))) as src:
        with closing(sqlite3.connect(str(dest))) as dst:
            src.backup(dst, pages=256)


def _open_snapshot(path):
    conn = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro&immutable=1", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def _required_files(conn):
    files = {"db.sqlite"}
    for doc in conn.execute("SELECT * FROM documents"):
        h = doc["doc_hash"]
        ext = Path(doc["original_filename"]).suffix.lower()
        if not HASH.fullmatch(h) or ext not in (".pdf", ".docx", ".txt", ".md"):
            raise ValueError("备份文档标识或文件类型不合法")
        files.update((f"files/{h}{ext}", f"text/{h}.txt"))
    for pkg in conn.execute("SELECT * FROM packages"):
        sha = pkg["sha256"]
        if not HASH.fullmatch(sha):
            raise ValueError("备份资料包哈希不合法")
        files.add(f"packages/{sha}.zip")
        name = f"pkg-{pkg['id']}" if pkg["status"] == "published" else f"pkg-draft-{sha[:16]}"
        files.add(f"vectors/{name}.npy")
    return files


def _check_snapshot(root):
    conn = _open_snapshot(root / "db.sqlite")
    try:
        required = {"settings", "packages", "documents", "sections", "doc_tags", "chunks", "chunks_fts", "vector_rows", "audit_log"}
        tables = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not required <= tables:
            raise ValueError("备份数据库表结构不完整")
        if conn.execute("PRAGMA integrity_check").fetchone()[0] != "ok" or conn.execute("PRAGMA foreign_key_check").fetchone():
            raise ValueError("备份数据库完整性校验失败")
        files = _required_files(conn)
        if any(not (root / name).is_file() for name in files):
            raise ValueError("备份缺少数据库引用的资料、资料包或向量文件")
        secret = conn.execute("SELECT value FROM settings WHERE key='token_secret'").fetchone()
        if secret is None or not HASH.fullmatch(secret[0]):
            raise ValueError("备份缺少有效的令牌签名设置")
        for doc in conn.execute("SELECT * FROM documents"):
            original = root / "files" / (doc["doc_hash"] + Path(doc["original_filename"]).suffix.lower())
            text = root / "text" / f"{doc['doc_hash']}.txt"
            if hash_file(original) != doc["doc_hash"] or hash_file(text) != doc["text_sha256"]:
                raise ValueError("备份原文件或标准化原文哈希不匹配")
            with text.open(encoding="utf-8") as source:
                line_count = sum(1 for _ in source)
            if line_count != doc["line_count"]:
                raise ValueError("备份原文行号不匹配")
        if conn.execute("SELECT c.id FROM chunks c LEFT JOIN vector_rows v ON c.id=v.chunk_id WHERE v.chunk_id IS NULL LIMIT 1").fetchone():
            raise ValueError("备份分块缺少向量关联")
        if conn.execute("SELECT v.chunk_id FROM vector_rows v LEFT JOIN chunks c ON c.id=v.chunk_id LEFT JOIN documents d ON d.id=c.doc_id WHERE c.id IS NULL OR v.doc_id!=c.doc_id OR v.package_id!=d.package_id LIMIT 1").fetchone():
            raise ValueError("备份向量关联不一致")
        if conn.execute("SELECT c.id FROM chunks c LEFT JOIN chunks_fts f ON f.rowid=c.id WHERE f.rowid IS NULL LIMIT 1").fetchone():
            raise ValueError("备份全文索引不完整")
        for pkg in conn.execute("SELECT * FROM packages"):
            from .pkgfmt import validate_identity
            validate_identity(dict(pkg))
            if hash_file(root / "packages" / f"{pkg['sha256']}.zip") != pkg["sha256"]:
                raise ValueError("备份资料包哈希不匹配")
            name = f"pkg-{pkg['id']}" if pkg["status"] == "published" else f"pkg-draft-{pkg['sha256'][:16]}"
            arr = np.load(root / "vectors" / f"{name}.npy", mmap_mode="r", allow_pickle=False)
            try:
                if arr.dtype != np.float32 or arr.shape != (pkg["chunk_count"], pkg["embed_dim"]):
                    raise ValueError("备份向量形状不匹配")
                for start in range(0, len(arr), 1024):
                    block = arr[start:start + 1024]
                    if not np.isfinite(block).all() or not np.allclose(np.linalg.norm(block, axis=1), 1, atol=1e-3):
                        raise ValueError("备份向量不是有限的归一化向量")
                if pkg["status"] == "published":
                    rows = conn.execute("SELECT row_index FROM vector_rows WHERE package_id=? ORDER BY row_index", (pkg["id"],))
                    count = 0
                    for count, row in enumerate(rows, 1):
                        if row[0] != count - 1:
                            raise ValueError("备份向量行号不连续")
                    if count != len(arr):
                        raise ValueError("备份向量数量不匹配")
            finally:
                if getattr(arr, "_mmap", None) is not None:
                    arr._mmap.close()
        counts = {table: conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0] for table in ("documents", "chunks")}
        return files, counts
    finally:
        conn.close()


def create_backup(db, out_path):
    data_dir = config.data_dir.resolve()
    require_space(data_dir.parent, db.path.stat().st_size * 2)
    with tempfile.TemporaryDirectory(prefix="cpb-backup-", dir=data_dir.parent) as tmp:
        root = Path(tmp)
        # Capture immutable file links while publish/discard are excluded, then release writes.
        with db.files_lock:
            with db.sql_lock:
                _snapshot_db(db.path, root / "db.sqlite")
            conn = _open_snapshot(root / "db.sqlite")
            try:
                names = _required_files(conn)
                counts = {t: conn.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0] for t in ("documents", "chunks")}
            finally:
                conn.close()
            for name in names - {"db.sqlite"}:
                dest = root / name
                dest.parent.mkdir(parents=True, exist_ok=True)
                os.link(data_dir / name, dest)
        size = sum((root / n).stat().st_size for n in names)
        require_space(data_dir.parent, size + size // 100 + 1024**2)
        meta = {"format": BACKUP_FORMAT, "version": BACKUP_VERSION, "created_at": utcnow(), "counts": counts,
            "files": {n: {"size": (root / n).stat().st_size, "sha256": hash_file(root / n)} for n in sorted(names)}}
        try:
            with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as zf:
                zf.writestr("backup.json", json.dumps(meta, ensure_ascii=False))
                for name in sorted(names):
                    zf.write(root / name, name)
        except BaseException:
            Path(out_path).unlink(missing_ok=True)
            raise
    return Path(out_path)


def _inspect_archive(zf):
    infos = zf.infolist()
    if len(infos) > MAX_ENTRIES or len({i.filename for i in infos}) != len(infos):
        raise ValueError("备份条目过多或重复")
    total = 0
    for info in infos:
        name = info.filename
        parts = PurePosixPath(name).parts
        if ("\\" in name or ":" in name or name.startswith("/") or any(p in ("", ".", "..") for p in name.split("/"))
                or (info.external_attr >> 16) & 0o170000 == 0o120000
                or not (name in ("backup.json", "db.sqlite") or len(parts) == 2 and parts[0] in SUBDIRS)):
            raise ValueError("备份包含不合法路径或符号链接")
        total += info.file_size
    if total > MAX_EXPANDED or total / max(1, sum(i.compress_size for i in infos)) > 200:
        raise ValueError("备份解压规模或压缩比超限")
    if "backup.json" not in zf.namelist() or zf.getinfo("backup.json").file_size > MAX_META:
        raise ValueError("备份清单缺失或超限")
    meta = json.loads(zf.read("backup.json"))
    if not isinstance(meta, dict) or meta.get("format") != BACKUP_FORMAT or meta.get("version") != BACKUP_VERSION:
        raise ValueError("备份格式或版本不支持，请使用新版程序重新导出")
    inventory = meta.get("files")
    if not isinstance(inventory, dict) or set(inventory) != set(zf.namelist()) - {"backup.json"}:
        raise ValueError("备份清单与文件条目不一致")
    for name, record in inventory.items():
        if not isinstance(record, dict) or record.get("size") != zf.getinfo(name).file_size or not HASH.fullmatch(str(record.get("sha256", ""))):
            raise ValueError("备份文件大小或哈希记录不合法")
    return meta, total


@contextmanager
def validated_backup(data):
    parent = config.data_dir.resolve().parent
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cpb-restore-", dir=parent) as tmp:
        root = Path(tmp)
        try:
            with zipfile.ZipFile(io.BytesIO(data) if isinstance(data, bytes) else data) as zf:
                meta, total = _inspect_archive(zf)
                require_space(parent, total)
                for name, expected in meta["files"].items():
                    dest = root / name
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    with zf.open(name) as source, dest.open("wb") as target:
                        shutil.copyfileobj(source, target, BLOCK_BYTES)
                    if hash_file(dest) != expected["sha256"]:
                        raise ValueError("备份文件哈希校验失败")
            names, counts = _check_snapshot(root)
            if names != set(meta["files"]) or counts != meta.get("counts"):
                raise ValueError("备份内容与数据库引用不一致")
        except (sqlite3.Error, zipfile.BadZipFile, KeyError, TypeError, UnicodeError, OSError) as exc:
            raise ValueError("备份内容损坏或读取失败") from exc
        yield root, meta


def validate_backup(data):
    with validated_backup(data) as (_, meta):
        return meta


def restore_backup(db, data, close_caches=None):
    with validated_backup(data) as (root, meta):
        return install_backup(db, root, meta, close_caches)


def install_backup(db, root, meta, close_caches=None):
    current = config.data_dir.resolve()
    if Path(db.path).resolve() != current / "campus.db":
        raise ValueError("恢复目标数据库路径不一致")
    (root / "db.sqlite").rename(root / "campus.db")
    for name in SUBDIRS:
        (root / name).mkdir(exist_ok=True)
    previous = current.parent / f"{current.name}.rollback-{uuid.uuid4().hex}"
    failed = current.parent / f"{current.name}.failed-{uuid.uuid4().hex}"
    with db.files_lock:
        if close_caches:
            close_caches()
        db.close()
        moved_old = False
        installed = False
        try:
            current.rename(previous)
            moved_old = True
            root.rename(current)
            installed = True
            db.reopen()
            db.one("SELECT COUNT(*) FROM documents")
        except BaseException:
            try:
                db.close()
                if installed:
                    current.rename(failed)
                if moved_old:
                    previous.rename(current)
                db.reopen()
            except BaseException as rollback_error:
                # Never delete previous/failed when rollback itself fails.
                raise RuntimeError("恢复回滚失败，旧数据目录已保留，请停止服务后人工恢复") from rollback_error
            if failed.exists():
                remove_tree(current.parent, failed)
            raise
    remove_tree(current.parent, previous)
    return meta["counts"]
