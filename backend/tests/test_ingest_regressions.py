import asyncio
import io
import json
import sqlite3
import threading
import zipfile
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from app import api, ingest, pkgfmt
from app.config import config
from app.pkgfmt import PackageError, validated_package
from app.storage import hash_file
from conftest import make_package


def mutate_package(data, fn):
    with zipfile.ZipFile(io.BytesIO(data)) as zf:
        entries = {n: zf.read(n) for n in zf.namelist()}
    fn(entries)
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, blob in entries.items():
            zf.writestr(name, blob)
    return out.getvalue()


@pytest.mark.parametrize("identity", [{"model": "wrong"}, {"dim": 512}, {"pre": "unknown"}])
def test_reject_identity_before_import(db, identity):
    with pytest.raises(ingest.IngestError, match="不匹配"):
        ingest.import_package(db, make_package(**identity), "wrong.zip")
    assert db.one("SELECT COUNT(*) c FROM packages")["c"] == 0


@pytest.mark.parametrize("value", [0, 2, float("nan"), float("inf")])
def test_reject_invalid_vector_norm(db, value):
    def mutate(entries):
        arr = np.load(io.BytesIO(entries["vectors.npy"]))
        arr[0, 0] = value
        buf = io.BytesIO()
        np.save(buf, arr)
        entries["vectors.npy"] = buf.getvalue()
    with pytest.raises(ingest.IngestError):
        ingest.import_package(db, mutate_package(make_package(), mutate), "invalid.zip")


def test_failed_target_correctable_without_reimport(db):
    pid = ingest.import_package(db, make_package(), "retry.zip")
    h = ingest.preview_package(db, pid)["documents"][0]["doc_hash"]
    with pytest.raises(ingest.IngestError, match="替代目标"):
        ingest.publish_package(db, pid, {h: "missing"})
    ingest.publish_package(db, pid, {})
    assert db.one("SELECT status FROM packages WHERE id=?", (pid,))["status"] == "published"


def test_commit_failure_removes_new_files_and_retries(db, monkeypatch):
    pid = ingest.import_package(db, make_package(), "retry.zip")
    commit = ingest._commit_documents
    def fail(*args):
        raise sqlite3.OperationalError("injected commit failure")
    monkeypatch.setattr(ingest, "_commit_documents", fail)
    with pytest.raises(sqlite3.OperationalError):
        ingest.publish_package(db, pid, {})
    assert not list((config.data_dir / "files").iterdir())
    assert not list((config.data_dir / "text").iterdir())
    assert not (config.data_dir / "vectors" / f"pkg-{pid}.npy").exists()
    monkeypatch.setattr(ingest, "_commit_documents", commit)
    ingest.publish_package(db, pid, {})
    assert db.one("SELECT COUNT(*) c FROM documents")["c"] == 5


def test_import_file_failure_leaves_no_orphan(db, monkeypatch):
    copy = ingest.shutil.copyfile
    def fail(source, target, *args, **kwargs):
        if Path(target).name.startswith("pkg-draft-"):
            raise OSError("injected disk full")
        return copy(source, target, *args, **kwargs)
    monkeypatch.setattr(ingest.shutil, "copyfile", fail)
    with pytest.raises(ingest.IngestError):
        ingest.import_package(db, make_package(), "fail.zip")
    assert not list((config.data_dir / "packages").iterdir())
    assert db.one("SELECT COUNT(*) c FROM packages")["c"] == 0


def test_low_space_rejects_before_extract(db, monkeypatch):
    monkeypatch.setattr("app.storage.shutil.disk_usage", lambda _: SimpleNamespace(free=0))
    with pytest.raises(ingest.IngestError):
        ingest.import_package(db, make_package(), "space.zip")
    assert db.one("SELECT COUNT(*) c FROM packages")["c"] == 0


def test_file_input_never_reads_entire_zip_and_preserves_bytes(db, tmp_path, monkeypatch):
    source = tmp_path / "package.zip"
    source.write_bytes(make_package())
    original = Path.read_bytes
    def reject_archive_read(path):
        if path.suffix == ".zip":
            raise AssertionError("whole archive read")
        return original(path)
    monkeypatch.setattr(Path, "read_bytes", reject_archive_read)
    pid = ingest.import_package(db, source, source.name)
    ingest.publish_package(db, pid, {})
    for doc in db.q("SELECT * FROM documents"):
        assert hash_file(config.data_dir / "text" / f"{doc['doc_hash']}.txt") == doc["text_sha256"]


def test_manifest_size_is_bounded(monkeypatch):
    monkeypatch.setattr(pkgfmt, "MAX_MANIFEST_BYTES", 64)
    with pytest.raises(PackageError, match="manifest"):
        with validated_package(make_package()):
            pass


def test_storage_worker_yields_event_loop_and_rejects_overlap():
    async def run():
        started, release = threading.Event(), threading.Event()
        def slow():
            started.set()
            release.wait(3)
            return 42
        task = asyncio.create_task(api.storage_call(slow))
        try:
            while not started.is_set():
                await asyncio.sleep(0.01)
            with pytest.raises(Exception) as exc:
                await api.storage_call(lambda: None)
            assert exc.value.status_code == 409
        finally:
            release.set()
        assert await task == 42
    asyncio.run(run())


def test_cancelled_storage_request_keeps_lock_until_worker_finishes():
    async def run():
        started, release = threading.Event(), threading.Event()
        def slow():
            started.set()
            release.wait(3)
        task = asyncio.create_task(api.storage_call(slow))
        try:
            while not started.is_set():
                await asyncio.sleep(0.01)
            task.cancel()
            await asyncio.sleep(0.02)
            assert api.storage_busy and not task.done()
            with pytest.raises(Exception) as exc:
                await api.storage_call(lambda: None)
            assert exc.value.status_code == 409
        finally:
            release.set()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert not api.storage_busy
    asyncio.run(run())
