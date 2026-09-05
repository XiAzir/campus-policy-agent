import asyncio
import io
import json
import zipfile
from pathlib import Path

import pytest

from app import backup, ingest
from app.config import config
from app.db import Database
from app.security import Tokens
from app.storage import hash_file
from conftest import make_package


def snapshot(db, tmp_path, *, draft=False):
    Tokens(db)
    pid = ingest.import_package(db, make_package(), "regression.zip")
    if not draft:
        ingest.publish_package(db, pid, {})
    archive = tmp_path / "backup.zip"
    backup.create_backup(db, archive)
    return archive, pid


def mutate_archive(path, change):
    with zipfile.ZipFile(path) as zf:
        entries = {n: zf.read(n) for n in zf.namelist()}
    change(entries)
    result = io.BytesIO()
    with zipfile.ZipFile(result, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return result.getvalue()


@pytest.mark.parametrize("draft", [False, True])
def test_restore_to_fresh_directory_preserves_packages(db, tmp_path, monkeypatch, draft):
    archive, pid = snapshot(db, tmp_path, draft=draft)
    monkeypatch.setattr(config, "data_dir", tmp_path / "fresh")
    fresh = Database(config.data_dir / "campus.db")
    try:
        backup.restore_backup(fresh, archive)
        assert len(ingest.preview_package(fresh, pid)["documents"]) == 5
        if draft:
            ingest.publish_package(fresh, pid, {})
        assert fresh.one("SELECT COUNT(*) c FROM documents")["c"] == 5
    finally:
        fresh.close()


@pytest.mark.parametrize("attack", ["missing-assets", "tamper", "version", "traversal", "symlink"])
def test_invalid_backup_does_not_touch_live_data(db, tmp_path, attack):
    archive, _ = snapshot(db, tmp_path)
    db.setting_set("marker", "live")
    before = {p.relative_to(config.data_dir).as_posix(): hash_file(p) for p in (config.data_dir / "files").iterdir()}

    def change(entries):
        meta = json.loads(entries["backup.json"])
        if attack == "missing-assets":
            for name in list(entries):
                if name.startswith(("files/", "text/", "vectors/", "packages/")):
                    entries.pop(name)
                    meta["files"].pop(name)
        elif attack == "tamper":
            name = next(n for n in entries if n.startswith("files/"))
            entries[name] += b"tampered"
        elif attack == "version":
            meta["version"] = 999
        elif attack == "traversal":
            entries["files/../escape"] = b"x"
        entries["backup.json"] = json.dumps(meta).encode()

    data = mutate_archive(archive, change)
    if attack == "symlink":
        buffer = io.BytesIO(data)
        with zipfile.ZipFile(buffer, "a") as zf:
            item = zipfile.ZipInfo("files/link")
            item.external_attr = 0o120777 << 16
            zf.writestr(item, "outside")
        data = buffer.getvalue()
    with pytest.raises(ValueError):
        backup.restore_backup(db, data)
    assert db.setting_get("marker") == "live"
    assert before == {p.relative_to(config.data_dir).as_posix(): hash_file(p) for p in (config.data_dir / "files").iterdir()}


@pytest.mark.parametrize("stage", ["move-old", "install-new", "reopen"])
def test_each_install_failure_rolls_back_database_and_files(db, tmp_path, monkeypatch, stage):
    archive, _ = snapshot(db, tmp_path)
    db.setting_set("marker", "newer-than-backup")
    proof = config.data_dir / "files" / "rollback-proof.txt"
    proof.write_text("old-live-data")
    rename = Path.rename
    reopen = db.reopen
    failed = False

    def guarded_rename(source, target):
        nonlocal failed
        should_fail = stage == "move-old" and ".rollback-" in Path(target).name or stage == "install-new" and source.name.startswith("cpb-restore-")
        if should_fail and not failed:
            failed = True
            raise OSError("injected failure")
        return rename(source, target)

    def guarded_reopen():
        nonlocal failed
        if stage == "reopen" and not failed:
            failed = True
            raise OSError("injected reopen failure")
        return reopen()

    monkeypatch.setattr(Path, "rename", guarded_rename)
    monkeypatch.setattr(db, "reopen", guarded_reopen)
    with pytest.raises(OSError):
        backup.restore_backup(db, archive)
    assert db.setting_get("marker") == "newer-than-backup"
    assert proof.read_text() == "old-live-data"


def test_failed_rollback_preserves_original_directory(db, tmp_path, monkeypatch):
    archive, _ = snapshot(db, tmp_path)
    proof = config.data_dir / "files" / "proof.txt"
    proof.write_text("original")
    rename = Path.rename

    def fail(source, target):
        if source.name.startswith("cpb-restore-") or ".rollback-" in source.name:
            raise OSError("injected persistent failure")
        return rename(source, target)

    monkeypatch.setattr(Path, "rename", fail)
    with pytest.raises(RuntimeError, match="回滚失败"):
        backup.restore_backup(db, archive)
    preserved = list(config.data_dir.parent.glob("data.rollback-*"))
    assert len(preserved) == 1
    assert (preserved[0] / "files" / "proof.txt").read_text() == "original"
    assert backup.restore_marker().exists()
    rename(preserved[0], config.data_dir)
    db.reopen()


def test_restore_rejects_embedding_identity_mismatch(db, tmp_path, monkeypatch):
    archive, _ = snapshot(db, tmp_path)
    monkeypatch.setattr(config, "siliconflow_model", "another-model")
    with pytest.raises(ValueError, match="当前配置"):
        backup.restore_backup(db, archive)
    assert db.one("SELECT COUNT(*) c FROM documents")["c"] == 5
    assert not backup.restore_marker().exists()


def test_incomplete_restore_blocks_startup(tmp_path):
    from app.main import app, lifespan
    backup.restore_marker().write_text("incomplete")
    async def run():
        with pytest.raises(RuntimeError, match="上次恢复未完成"):
            async with lifespan(app):
                pass
    asyncio.run(run())
    assert not (config.data_dir / "campus.db").exists()


def test_backup_snapshot_does_not_depend_on_later_file_deletion(db, tmp_path, monkeypatch):
    Tokens(db)
    pid = ingest.import_package(db, make_package(), "draft.zip")
    package = ingest.package_path(db.one("SELECT sha256 FROM packages WHERE id=?", (pid,))["sha256"])
    original_hash = backup.hash_file
    deleted = False

    def hash_after_delete(path):
        nonlocal deleted
        if not deleted:
            package.unlink()
            deleted = True
        return original_hash(path)

    monkeypatch.setattr(backup, "hash_file", hash_after_delete)
    archive = tmp_path / "backup.zip"
    backup.create_backup(db, archive)
    assert backup.validate_backup(archive)["counts"]["documents"] == 0
