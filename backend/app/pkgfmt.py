"""服务端资料包格式校验（与 skill/scripts 同一规范，独立实现：服务器不信任任何输入）。"""

from __future__ import annotations

import hashlib
import io
import json
import re
import zipfile
import shutil
import tempfile
from contextlib import contextmanager
from datetime import date
from pathlib import Path

import numpy as np
from .config import config
from .storage import BLOCK_BYTES, hash_file, require_space

FORMAT_VERSION = 1
MAX_PACKAGE_BYTES = 200 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED = 2 * 1024 * 1024 * 1024
MAX_RATIO = 200
MAX_MANIFEST_BYTES = 8 * 1024 * 1024
MAX_TEXT_BYTES = 4 * 1024 * 1024
MAX_CHUNKS = 20_000
ALLOWED_TOP = {"manifest.json", "vectors.npy", "files", "text"}
ALLOWED_PREFIXES = ("files/", "text/")
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
DOC_TYPES = {"pdf", "docx", "md", "txt"}


class PackageError(Exception):
    pass


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def check_zip_safety(zf: zipfile.ZipFile) -> int:
    names = zf.namelist()
    if len(names) > 10_002 or len(names) != len(set(names)):
        raise PackageError("条目过多或存在重复路径")
    top = {n.split("/", 1)[0] if "/" in n else n for n in names}
    for n in top:
        if n not in ALLOWED_TOP:
            raise PackageError(f"包含不允许的条目：{n}")
    total = 0
    for info in zf.infolist():
        name = info.filename
        if "\\" in name or ":" in name or name.startswith("/") or re.match(r"^[A-Za-z]:", name):
            raise PackageError(f"条目路径不合法：{name!r}")
        parts = name.split("/")
        if any(p in ("..", ".", "") for p in parts):
            raise PackageError(f"条目含 .. 穿越组件：{name!r}")
        if name not in ("files", "text") and "/" in name and not name.startswith(ALLOWED_PREFIXES):
            raise PackageError(f"条目不在白名单前缀内：{name!r}")
        mode = (info.external_attr >> 16) & 0o170000
        if mode == 0o120000:
            raise PackageError(f"条目是符号链接：{name!r}")
        total += info.file_size
    if total > MAX_TOTAL_UNCOMPRESSED:
        raise PackageError(f"解压总量超过上限 {MAX_TOTAL_UNCOMPRESSED}")
    compressed = sum(i.compress_size for i in zf.infolist()) or 1
    if total / compressed > MAX_RATIO:
        raise PackageError("压缩比异常（疑似解压炸弹）")
    return total


def _validate_manifest(manifest: dict) -> list[dict]:
    if manifest.get("format_version") != FORMAT_VERSION:
        raise PackageError(f"format_version 必须是 {FORMAT_VERSION}")
    if not isinstance(manifest.get("preprocessing_version"), str) or not manifest["preprocessing_version"]:
        raise PackageError("preprocessing_version 缺失")
    if not isinstance(manifest.get("embed_model"), str) or not manifest["embed_model"]:
        raise PackageError("embed_model 缺失")
    if not isinstance(manifest.get("embed_dim"), int) or not 1 <= manifest["embed_dim"] <= 8192:
        raise PackageError("embed_dim 必须是正整数")
    docs = manifest.get("documents")
    if not isinstance(docs, list) or not docs:
        raise PackageError("documents 必须是非空数组")

    seen: set[str] = set()
    expected_vec = 0
    for i, doc in enumerate(docs):
        where = f"documents[{i}]"
        if not isinstance(doc, dict):
            raise PackageError(f"{where} 必须是对象")
        h = doc.get("doc_hash", "")
        if not isinstance(h, str) or not HASH_RE.fullmatch(h):
            raise PackageError(f"{where}.doc_hash 不合法")
        if h in seen:
            raise PackageError(f"包内重复 doc_hash：{h[:12]}…")
        seen.add(h)
        if doc.get("doc_type") not in DOC_TYPES:
            raise PackageError(f"{where}.doc_type 不合法")
        for key in ("title", "original_filename", "department"):
            if not isinstance(doc.get(key), str) or not doc[key].strip():
                raise PackageError(f"{where}.{key} 必须是非空字符串")
        filename = doc["original_filename"]
        if any(c in filename for c in ("/", "\\", ":")) or Path(filename).suffix.lower() != "." + doc["doc_type"]:
            raise PackageError(f"{where}.original_filename 类型或路径不合法")
        if doc.get("effective_date") is not None and not (
            isinstance(doc.get("effective_date"), str) and DATE_RE.match(doc["effective_date"])
        ):
            raise PackageError(f"{where}.effective_date 必须是 YYYY-MM-DD 或 null")
        if doc.get("effective_date"):
            try:
                date.fromisoformat(doc["effective_date"])
            except ValueError as exc:
                raise PackageError("生效日期不存在") from exc
        if not isinstance(doc.get("audience"), list) or not all(
            isinstance(a, str) and a.strip() for a in doc["audience"]
        ):
            raise PackageError(f"{where}.audience 不合法")
        if not isinstance(doc.get("line_count"), int) or doc["line_count"] < 1:
            raise PackageError(f"{where}.line_count 不合法")
        if not isinstance(doc.get("notes", ""), str) or len(doc.get("notes", "")) > 4000:
            raise PackageError("备注必须是有界字符串")
        if doc.get("replaces") is not None and not isinstance(doc["replaces"], str):
            raise PackageError("替代目标必须是文档标识或空值")
        if doc["doc_type"] == "pdf":
            pm = doc.get("page_map")
            if not isinstance(pm, list) or not pm or pm[0] != [1, 1]:
                raise PackageError(f"{where}.page_map（PDF）必须以 [1,1] 开头")
            last = (0, 0)
            for pair in pm:
                if (
                    not isinstance(pair, list)
                    or len(pair) != 2
                    or not all(isinstance(x, int) for x in pair)
                    or pair[0] <= last[0]
                    or pair[1] < last[1]
                ):
                    raise PackageError(f"{where}.page_map 区段必须按行号升序且页码不减")
                last = pair
        if not isinstance(doc.get("text_sha256"), str) or not HASH_RE.match(doc["text_sha256"]):
            raise PackageError(f"{where}.text_sha256 不合法")

        section_ids = set()
        for sec in doc.get("sections", []):
            if not isinstance(sec, dict) or not sec.get("section_id"):
                raise PackageError(f"{where}.sections 不合法")
            if sec["section_id"] in section_ids:
                raise PackageError(f"{where} 章节重复：{sec['section_id']}")
            section_ids.add(sec["section_id"])
            if not (1 <= sec.get("start_line", 0) <= sec.get("end_line", 0) <= doc["line_count"]):
                raise PackageError(f"{where} 章节 {sec['section_id']} 行区间越界")
        for dom in doc.get("domains", []):
            if not isinstance(dom, dict) or not isinstance(dom.get("tag"), str) or not dom["tag"].strip():
                raise PackageError(f"{where}.domains.tag 不合法")
            for sid in dom.get("section_ids", []):
                if sid not in section_ids:
                    raise PackageError(f"{where} 领域 {dom['tag']} 引用不存在章节 {sid}")
        chunks = doc.get("chunks")
        if not isinstance(chunks, list) or not chunks:
            raise PackageError(f"{where}.chunks 必须是非空数组")
        prev_end = 0
        for ch in chunks:
            vi = ch.get("vector_index")
            if not isinstance(vi, int) or vi != expected_vec:
                raise PackageError(f"{where} chunk vector_index 必须从 0 开始连续")
            expected_vec += 1
            if not (1 <= ch.get("line_start", 0) <= ch.get("line_end", 0) <= doc["line_count"]):
                raise PackageError(f"{where} chunk {vi} 行区间越界")
            if ch["line_start"] < prev_end:
                raise PackageError(f"{where} chunk {vi} 行区间与前一 chunk 重叠")
            prev_end = ch["line_end"]
            if not isinstance(ch.get("text"), str) or not ch["text"]:
                raise PackageError(f"{where} chunk {vi} text 缺失")
            sid = ch.get("section_id")
            if sid is not None and sid not in section_ids:
                raise PackageError(f"{where} chunk {vi} 引用不存在章节 {sid}")

    vec = manifest.get("vectors")
    if not isinstance(vec, dict) or vec.get("file") != "vectors.npy":
        raise PackageError("vectors.file 必须是 vectors.npy")
    if vec.get("dtype") != "float32":
        raise PackageError("vectors.dtype 必须是 float32")
    if vec.get("count") != expected_vec:
        raise PackageError("vectors.count 与 chunk 总数不一致")
    if expected_vec > MAX_CHUNKS:
        raise PackageError("分块数量超限，请分包")
    if vec.get("dim") != manifest["embed_dim"]:
        raise PackageError("vectors.dim 与 embed_dim 不一致")
    return docs




def validate_identity(manifest):
    if (manifest["embed_model"], manifest["embed_dim"], manifest["preprocessing_version"]) != (
        config.siliconflow_model, config.embed_dims, config.preprocessing_version
    ):
        raise PackageError("向量模型、维度或预处理版本与服务配置不匹配，必须重新生成资料包")


@contextmanager
def validated_package(data, *, check_identity=True):
    """Only metadata and one bounded text document are materialized in memory."""
    parent = config.data_dir.parent
    parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cpb-package-", dir=parent) as tmp:
        root = Path(tmp)
        if isinstance(data, bytes):
            path = root / "upload.zip"
            path.write_bytes(data)
        else:
            path = Path(data)
        try:
            if path.stat().st_size > min(MAX_PACKAGE_BYTES, config.max_package_mb * 1024**2):
                raise PackageError("包大小超过单包上限，请分包")
            with zipfile.ZipFile(path) as zf:
                total = check_zip_safety(zf)
                if "manifest.json" not in zf.namelist() or "vectors.npy" not in zf.namelist():
                    raise PackageError("缺少 manifest.json 或 vectors.npy")
                if zf.getinfo("manifest.json").file_size > MAX_MANIFEST_BYTES:
                    raise PackageError("manifest 大小超限，请分包")
                manifest = json.loads(zf.read("manifest.json"))
                if not isinstance(manifest, dict):
                    raise PackageError("manifest 顶层必须是对象")
                docs = _validate_manifest(manifest)
                if check_identity:
                    validate_identity(manifest)
                names = {"manifest.json", "vectors.npy"}
                for doc in docs:
                    names.update((f"files/{doc['doc_hash']}{Path(doc['original_filename']).suffix.lower()}", f"text/{doc['doc_hash']}.txt"))
                if names != set(zf.namelist()):
                    raise PackageError("包内条目与清单不一致")
                # Allow staging, final files, FTS/index growth and transaction rollback.
                require_space(parent, total * 2 + MAX_MANIFEST_BYTES * 6 + path.stat().st_size)
                for name in sorted(names - {"manifest.json"}):
                    if name.startswith("text/") and zf.getinfo(name).file_size > MAX_TEXT_BYTES:
                        raise PackageError("单份标准化原文超限，请拆分文档")
                    dest = root / name
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    with zf.open(name) as source, dest.open("wb") as target:
                        shutil.copyfileobj(source, target, BLOCK_BYTES)
            arr = np.load(root / "vectors.npy", mmap_mode="r", allow_pickle=False)
            try:
                if arr.dtype != np.float32 or arr.shape != (manifest["vectors"]["count"], manifest["embed_dim"]):
                    raise PackageError("向量 dtype 或形状与清单不匹配")
                for start in range(0, len(arr), 1024):
                    block = arr[start:start + 1024]
                    if not np.isfinite(block).all():
                        raise PackageError("向量包含非有限值")
                    norms = np.linalg.norm(block, axis=1)
                    if np.any(norms <= 1e-6) or not np.allclose(norms, 1, atol=1e-3):
                        raise PackageError("向量必须非零且已归一化")
            finally:
                if getattr(arr, "_mmap", None) is not None:
                    arr._mmap.close()
            for doc in docs:
                h = doc["doc_hash"]
                original = root / "files" / (h + Path(doc["original_filename"]).suffix.lower())
                text = root / "text" / f"{h}.txt"
                if hash_file(original) != h or hash_file(text) != doc["text_sha256"]:
                    raise PackageError("原文件或标准化原文哈希不匹配")
                lines = text.read_text(encoding="utf-8").splitlines()
                if len(lines) != doc["line_count"]:
                    raise PackageError("标准化原文行数不匹配")
                for chunk in doc["chunks"]:
                    if chunk["text"] != "\n".join(lines[chunk["line_start"] - 1:chunk["line_end"]]):
                        raise PackageError("分块 text 与标准化原文不一致")
            result = {"manifest": manifest, "documents": docs, "root": root, "path": path,
                "sha256": hash_file(path), "size": path.stat().st_size, "expanded": total}
        except PackageError:
            raise
        except (ValueError, OSError, TypeError, AttributeError, KeyError, zipfile.BadZipFile) as exc:
            raise PackageError("资料包结构、向量或文件校验失败") from exc
        yield result


def validate_package(data):
    """Compatibility helper for local tests; HTTP ingestion uses validated_package."""
    with validated_package(data, check_identity=False) as pkg:
        root = pkg["root"]
        files = {p.relative_to(root).as_posix(): p.read_bytes() for p in (root / "files").iterdir()}
        texts = {p.relative_to(root).as_posix(): p.read_text(encoding="utf-8") for p in (root / "text").iterdir()}
        return pkg["manifest"], pkg["documents"], np.load(root / "vectors.npy", allow_pickle=False), files, texts
