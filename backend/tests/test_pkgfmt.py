"""资料包格式校验测试：正常包 + 恶意包拒绝。"""

import io
import json
import zipfile

import pytest

from app.pkgfmt import PackageError, validate_package


def repack(base: bytes, extra: dict | None = None, mutate=None) -> bytes:
    src = zipfile.ZipFile(io.BytesIO(base))
    contents = {n: src.read(n) for n in src.namelist()}
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for n, d in contents.items():
            d2 = mutate(n, d) if mutate else d
            if d2 is None:
                continue
            zf.writestr(n, d2)
        for n, d in (extra or {}).items():
            zf.writestr(n, d)
    return buf.getvalue()


def test_valid_package(real_package_bytes):
    manifest, docs, vecs, files, texts = validate_package(real_package_bytes)
    assert len(docs) == 5
    assert vecs.shape[1] == 1024
    assert sum(len(d["chunks"]) for d in docs) == manifest["vectors"]["count"]


def test_reject_path_traversal(real_package_bytes):
    bad = repack(real_package_bytes, extra={"files/../evil.txt": b"x"})
    with pytest.raises(PackageError, match="穿越"):
        validate_package(bad)


def test_reject_foreign_entry(real_package_bytes):
    bad = repack(real_package_bytes, extra={"evil.txt": b"x"})
    with pytest.raises(PackageError, match="不允许的条目"):
        validate_package(bad)


def test_reject_text_hash_tamper(real_package_bytes):
    bad = repack(real_package_bytes, mutate=lambda n, d: d + b"tampered" if n.startswith("text/") else d)
    with pytest.raises(PackageError, match="哈希不匹配|行数"):
        validate_package(bad)


def test_reject_symlink(real_package_bytes):
    src = zipfile.ZipFile(io.BytesIO(real_package_bytes))
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for n in src.namelist():
            zf.writestr(n, src.read(n))
        zi = zipfile.ZipInfo("files/link.txt")
        zi.external_attr = 0o120777 << 16
        zf.writestr(zi, "/etc/passwd")
    with pytest.raises(PackageError, match="符号链接"):
        validate_package(buf.getvalue())


def test_reject_zip_bomb_in_named_entry():
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        zf.writestr("manifest.json", json.dumps({"format_version": 1}))
        zf.writestr("vectors.npy", b"0" * (300 * 1024 * 1024))
    with pytest.raises(PackageError, match="炸弹|大小"):
        validate_package(buf.getvalue())


def test_reject_duplicate_doc_hash(real_package_bytes):
    def dup_manifest(n, d):
        if n != "manifest.json":
            return d
        m = json.loads(d)
        first = m["documents"][0]
        clone = json.loads(json.dumps(first))
        for i, c in enumerate(clone["chunks"]):
            c["vector_index"] = 1000 + i
        m["documents"].append(clone)
        m["vectors"]["count"] += len(clone["chunks"])
        return json.dumps(m).encode()

    with pytest.raises(PackageError, match="重复 doc_hash"):
        validate_package(repack(real_package_bytes, mutate=dup_manifest))
