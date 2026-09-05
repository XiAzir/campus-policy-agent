"""pytest fixtures：测试独立数据目录，不触碰真实 data/。"""

import os
import sys
import io
import json
import zipfile
from pathlib import Path

import numpy as np
import pytest

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "backend"))

os.environ["DATA_DIR"] = str(REPO / ".pytest_cache" / "unused-data")
os.environ["GEMINI_BASE_URL"] = "https://fake.local/v1beta"
os.environ["GEMINI_API_KEY"] = "fake"
os.environ["GEMINI_MODEL"] = "fake-model"
os.environ["SILICONFLOW_API_KEY"] = "fake"
os.environ["SILICONFLOW_EMBED_MODEL"] = "test-embed"
os.environ["INITIAL_ADMIN_PASSWORD"] = "admin"

from app.config import config  # noqa: E402
from app.db import Database  # noqa: E402
from app.vectors import get_index  # noqa: E402
from app.pkgfmt import sha256_bytes  # noqa: E402


@pytest.fixture(autouse=True)
def isolated_data(tmp_path, monkeypatch):
    from app.security import limiter
    from app.vectors import _index_singletons

    monkeypatch.setattr(config, "data_dir", tmp_path / "data")
    limiter._buckets.clear()
    yield
    for index in _index_singletons.values():
        index.close_all()
    _index_singletons.clear()


@pytest.fixture()
def db():
    path = config.data_dir / "campus.db"
    database = Database(path)
    yield database
    database.close()
    path.unlink(missing_ok=True)
    for suffix in ("-wal", "-shm"):
        Path(str(path) + suffix).unlink(missing_ok=True)


@pytest.fixture()
def vectors():
    return get_index(config.data_dir / "vectors")


@pytest.fixture()
def real_package_bytes() -> bytes:
    """Synthetic documents keep regression tests independent of private materials."""
    return make_package()


def make_package(label="", *, model="test-embed", dim=1024, pre="v1"):
    titles = ["三下乡报名通知", "社会实践安排", "社会实践评奖", "武汉理工大学社会实践基地管理办法（试行）", "社会实践安全"]
    documents, entries = [], {}
    for i, title in enumerate(titles):
        text = f"{title}{label}\n社会实践基地管理与报名要求 {i}。\n三下乡报名截止日期为6月22日。\n"
        blob = text.encode()
        h = sha256_bytes(blob)
        documents.append({
            "doc_hash": h, "original_filename": f"document-{i}.txt", "doc_type": "txt",
            "title": title + label, "department": "团委", "effective_date": "2026-06-01",
            "audience": ["全校"], "domains": [{"tag": "共青团", "section_ids": []}],
            "replaces": None, "notes": "", "line_count": 3, "page_map": None,
            "sections": [{"section_id": "s1", "title": title, "start_line": 1, "end_line": 3}],
            "text_sha256": h, "chunks": [{"chunk_id": 0, "text": text.rstrip("\n"),
                "line_start": 1, "line_end": 3, "section_id": "s1", "vector_index": i}],
        })
        entries[f"files/{h}.txt"] = blob
        entries[f"text/{h}.txt"] = blob
    vecs = np.zeros((5, dim), dtype=np.float32)
    for i in range(5):
        vecs[i, i % dim] = 1
    vec_buf = io.BytesIO()
    np.save(vec_buf, vecs)
    entries["vectors.npy"] = vec_buf.getvalue()
    entries["manifest.json"] = json.dumps({"format_version": 1, "preprocessing_version": pre,
        "embed_model": model, "embed_dim": dim, "documents": documents,
        "vectors": {"file": "vectors.npy", "dtype": "float32", "count": 5, "dim": dim}}, ensure_ascii=False).encode()
    result = io.BytesIO()
    with zipfile.ZipFile(result, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return result.getvalue()
