"""SQLite 数据层：模式、连接、FTS5（jieba 分词写入）。

聊天正文、工具结果、反馈一律不落库；本库只保存资料、版本、管理设置与审计记录。
"""

from __future__ import annotations

import json
import sqlite3
import threading
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS packages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sha256 TEXT UNIQUE NOT NULL,
  original_filename TEXT NOT NULL,
  size INTEGER NOT NULL,
  imported_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('draft','published')),
  preprocessing_version TEXT NOT NULL,
  embed_model TEXT NOT NULL,
  embed_dim INTEGER NOT NULL,
  doc_count INTEGER NOT NULL,
  chunk_count INTEGER NOT NULL,
  meta_overrides TEXT NOT NULL DEFAULT '{}',
  error TEXT
);

CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_uid TEXT UNIQUE NOT NULL,
  doc_hash TEXT NOT NULL,
  package_id INTEGER NOT NULL REFERENCES packages(id),
  title TEXT NOT NULL,
  original_filename TEXT NOT NULL,
  doc_type TEXT NOT NULL,
  department TEXT NOT NULL,
  effective_date TEXT,
  audience TEXT NOT NULL DEFAULT '[]',
  audience_scope TEXT NOT NULL DEFAULT '{}',
  notes TEXT NOT NULL DEFAULT '',
  line_count INTEGER NOT NULL,
  page_map TEXT,
  text_sha256 TEXT NOT NULL,
  replaces_doc_id INTEGER REFERENCES documents(id),
  published_at TEXT NOT NULL,
  deactivated_kind TEXT NOT NULL DEFAULT '' CHECK(deactivated_kind IN ('','superseded','manual')),
  deactivated_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_documents_kind ON documents(deactivated_kind);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(doc_hash);

CREATE TABLE IF NOT EXISTS sections (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  section_id TEXT NOT NULL,
  title TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  UNIQUE(doc_id, section_id)
);

CREATE TABLE IF NOT EXISTS doc_tags (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  section_id TEXT,
  tag TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_doc_tags_doc ON doc_tags(doc_id);
CREATE INDEX IF NOT EXISTS idx_doc_tags_tag ON doc_tags(tag);

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  chunk_index INTEGER NOT NULL,
  text TEXT NOT NULL,
  line_start INTEGER NOT NULL,
  line_end INTEGER NOT NULL,
  section_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(body);

CREATE TABLE IF NOT EXISTS vector_rows (
  chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id),
  row_index INTEGER NOT NULL,
  doc_id INTEGER NOT NULL,
  package_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vector_rows_doc ON vector_rows(doc_id);

CREATE TABLE IF NOT EXISTS audit_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at TEXT NOT NULL,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT ''
);
"""


def utcnow() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class Database:
    def __init__(self, path: Path):
        self.files_lock = threading.RLock()
        self.sql_lock = threading.RLock()
        self.read_lock = threading.RLock()
        self._transaction = threading.local()
        self.path = path
        path.parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(str(path), check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA foreign_keys=ON")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        self._conn.executescript(SCHEMA)
        if "audience_scope" not in {r[1] for r in self._conn.execute("PRAGMA table_info(documents)")}:
            self._conn.execute("ALTER TABLE documents ADD COLUMN audience_scope TEXT NOT NULL DEFAULT '{}'")
        self._conn.commit()
        self._open_reader()

    def _open_reader(self):
        self._reader = sqlite3.connect(self.path.resolve().as_uri() + "?mode=ro", uri=True, check_same_thread=False)
        self._reader.row_factory = sqlite3.Row

    @contextmanager
    def tx(self):
        """写事务；异常自动回滚。"""
        with self.sql_lock:
            previous = getattr(self._transaction, "active", False)
            self._transaction.active = True
            try:
                yield self._conn
                self._conn.commit()
            except BaseException:
                self._conn.rollback()
                raise
            finally:
                self._transaction.active = previous

    def q(self, sql: str, params: tuple = ()) -> list[sqlite3.Row]:
        if getattr(self._transaction, "active", False):
            return self._conn.execute(sql, params).fetchall()
        # WAL readers keep serving the committed catalog while publishing writes.
        with self.read_lock:
            return self._reader.execute(sql, params).fetchall()

    def one(self, sql: str, params: tuple = ()) -> sqlite3.Row | None:
        if getattr(self._transaction, "active", False):
            return self._conn.execute(sql, params).fetchone()
        with self.read_lock:
            return self._reader.execute(sql, params).fetchone()

    def setting_get(self, key: str) -> str | None:
        row = self.one("SELECT value FROM settings WHERE key=?", (key,))
        return row["value"] if row else None

    def setting_set(self, key: str, value: str) -> None:
        with self.tx() as conn:
            conn.execute(
                "INSERT INTO settings(key,value) VALUES(?,?) "
                "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (key, value),
            )

    def audit(self, actor: str, action: str, detail: str = "") -> None:
        """审计日志：只记动作与标识，不记问答正文与密钥。"""
        with self.tx() as conn:
            conn.execute(
                "INSERT INTO audit_log(at,actor,action,detail) VALUES(?,?,?,?)",
                (utcnow(), actor, action, detail[:500]),
            )

    def close(self) -> None:
        self._reader.close()
        self._conn.close()

    def reopen(self) -> None:
        """重新打开当前路径的数据库（恢复流程在替换文件后调用）。"""
        self._conn = sqlite3.connect(str(self.path), check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA foreign_keys=ON")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        self._open_reader()

    @property
    def conn(self) -> sqlite3.Connection:
        return self._conn

    # ---- 检索辅助：FTS 行维护 ----
    def fts_insert(self, conn, chunk_id: int, text: str) -> None:
        from .textutil import tokenize_for_fts

        conn.execute(
            "INSERT INTO chunks_fts(rowid, body) VALUES(?,?)",
            (chunk_id, tokenize_for_fts(text)),
        )

    def fts_delete(self, conn, chunk_id: int) -> None:
        conn.execute("DELETE FROM chunks_fts WHERE rowid=?", (chunk_id,))

    def document_public(self, row) -> dict:
        """documents 行 → 公开 JSON。"""
        tags = self.q("SELECT tag, section_id FROM doc_tags WHERE doc_id=?", (row["id"],))
        domains: dict[str, list[str]] = {}
        for t in tags:
            domains.setdefault(t["tag"], []).append(t["section_id"] or "")
        return {
            "doc_uid": row["doc_uid"],
            "title": row["title"],
            "doc_type": row["doc_type"],
            "department": row["department"],
            "effective_date": row["effective_date"],
            "audience": json.loads(row["audience"]),
            "audience_scope": json.loads(row["audience_scope"]),
            "doc_hash": row["doc_hash"],
            "replaces_doc_uid": (self.one("SELECT doc_uid FROM documents WHERE id=?", (row["replaces_doc_id"],)) or {"doc_uid": None})["doc_uid"],
            "domains": domains,
            "line_count": row["line_count"],
            "published_at": row["published_at"],
            "deactivated_kind": row["deactivated_kind"],
            "notes": row["notes"],
        }
