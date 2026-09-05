"""pytest fixtures：测试独立数据目录，不触碰真实 data/。"""

import os
import sys
import tempfile
from pathlib import Path

import uuid as _uuid

import pytest

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "backend"))

_test_dir = tempfile.mkdtemp(prefix="cpb-test-")
os.environ["DATA_DIR"] = _test_dir
os.environ.setdefault("GEMINI_BASE_URL", "https://fake.local/v1beta")
os.environ.setdefault("GEMINI_API_KEY", "fake")
os.environ.setdefault("GEMINI_MODEL", "fake-model")
os.environ.setdefault("SILICONFLOW_API_KEY", "fake")
os.environ.setdefault("SILICONFLOW_EMBED_MODEL", "fake-embed")

from app.config import config  # noqa: E402
from app.db import Database  # noqa: E402
from app.vectors import get_index  # noqa: E402


@pytest.fixture()
def db():
    path = config.data_dir / f"test-{_uuid.uuid4().hex}.db"
    database = Database(path)
    yield database
    database.close()
    path.unlink(missing_ok=True)
    for suffix in ("-wal", "-shm"):
        Path(str(path) + suffix).unlink(missing_ok=True)


@pytest.fixture()
def vectors():
    return get_index(config.data_dir / "vectors")


FIXTURE_PACKAGE = REPO.parent / "测试资料包-2026-09.zip"


@pytest.fixture()
def real_package_bytes() -> bytes:
    assert FIXTURE_PACKAGE.exists(), "先运行 skill 打包脚本生成 测试资料包-2026-09.zip"
    return FIXTURE_PACKAGE.read_bytes()
