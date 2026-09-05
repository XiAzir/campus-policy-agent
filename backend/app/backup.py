"""备份与恢复（plan 第三节）。

- 备份：SQLite 快照（在线 backup API，不暂停读写）+ 资料/向量/文本/管理设置，流式下载。
  不包含聊天（服务端本就不存）与 API 密钥（密钥在 .env，不入备份）。
- 恢复：上传校验 → 暂停写入 → 原子替换 → 失败回滚旧目录。
"""

from __future__ import annotations

import io
import json
import shutil
import sqlite3
import tempfile
import zipfile
from pathlib import Path

from .config import config

BACKUP_FORMAT = "campus-policy-backup"
BACKUP_VERSION = 1


def _snapshot_db(db_path: Path, dest: Path) -> None:
    src = sqlite3.connect(str(db_path))
    try:
        dst = sqlite3.connect(str(dest))
        try:
            with dst:
                src.backup(dst)
        finally:
            dst.close()
    finally:
        src.close()


def create_backup(db, out_path: Path) -> Path:
    """生成备份 zip，返回路径。db 为 app.db.Database。"""
    data_dir = config.data_dir
    data_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cpb-backup-") as tmp:
        snapshot = Path(tmp) / "db.sqlite"
        _snapshot_db(Path(db.path), snapshot)
        meta = {
            "format": BACKUP_FORMAT,
            "version": BACKUP_VERSION,
            "created_at": db.setting_get("_backup_at") or "",
            "counts": {
                "documents": db.one("SELECT COUNT(*) c FROM documents")["c"],
                "chunks": db.one("SELECT COUNT(*) c FROM chunks")["c"],
            },
        }
        with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as zf:
            zf.write(snapshot, "db.sqlite")
            zf.writestr("backup.json", json.dumps(meta, ensure_ascii=False, indent=2))
            for sub in ("files", "text", "vectors"):
                base = data_dir / sub
                if not base.exists():
                    continue
                for p in base.rglob("*"):
                    if p.is_file():
                        zf.write(p, f"{sub}/{p.relative_to(base).as_posix()}")
    return out_path


def validate_backup(data: bytes) -> dict:
    """恢复前校验，返回 backup.json 内容。"""
    try:
        zf = zipfile.ZipFile(io.BytesIO(data))
    except zipfile.BadZipFile as exc:
        raise ValueError("不是有效的备份文件") from exc
    with zf:
        names = zf.namelist()
        if "backup.json" not in names or "db.sqlite" not in names:
            raise ValueError("缺少 backup.json 或 db.sqlite")
        for n in names:
            if "\\" in n or n.startswith("/") or ".." in Path(n).parts:
                raise ValueError(f"备份内路径不合法：{n!r}")
        meta = json.loads(zf.read("backup.json"))
        if meta.get("format") != BACKUP_FORMAT:
            raise ValueError("备份格式标识不符")
    return meta


def restore_backup(db, data: bytes, close_caches=None) -> dict:
    """恢复：校验 → 暂停写入 → 替换 → 失败回滚。返回恢复摘要。"""
    meta = validate_backup(data)
    data_dir = config.data_dir
    data_dir.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix="cpb-restore-"))
    prev_dir = Path(tempfile.mkdtemp(prefix="cpb-prev-"))
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as zf:
            zf.extractall(staging)

        # 校验快照可用（能打开且有 documents 表）
        check = sqlite3.connect(str(staging / "db.sqlite"))
        try:
            tables = {r[0] for r in check.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if "documents" not in tables:
                raise ValueError("备份库缺少 documents 表")
        finally:
            check.close()

        # 暂停写入并释放向量 mmap，避免替换时文件被占用
        if close_caches:
            close_caches()
        db.close()
        try:
            # 旧数据挪走（首次恢复时可能还没有旧库）
            current_db = data_dir / "campus.db"
            if current_db.exists():
                (prev_dir / "db.sqlite").write_bytes(current_db.read_bytes())
            for sub in ("files", "text", "vectors"):
                src = data_dir / sub
                if src.exists():
                    shutil.move(str(src), str(prev_dir / sub))
            # 新数据就位
            shutil.copy(str(staging / "db.sqlite"), str(current_db))
            for sub in ("files", "text", "vectors"):
                src = staging / sub
                if src.exists():
                    shutil.move(str(src), str(data_dir / sub))
            db.reopen()
        except Exception:
            # 回滚：有旧库则还原旧库；首次恢复（无旧库）则移除半成品新库
            prev_db = prev_dir / "db.sqlite"
            if prev_db.exists():
                shutil.copy(str(prev_db), str(data_dir / "campus.db"))
            else:
                (data_dir / "campus.db").unlink(missing_ok=True)
            for sub in ("files", "text", "vectors"):
                if (prev_dir / sub).exists() and not (data_dir / sub).exists():
                    shutil.move(str(prev_dir / sub), str(data_dir / sub))
            db.reopen()
            raise
    finally:
        shutil.rmtree(staging, ignore_errors=True)
        shutil.rmtree(prev_dir, ignore_errors=True)
    return {"documents": meta.get("counts", {}).get("documents"), "chunks": meta.get("counts", {}).get("chunks")}
