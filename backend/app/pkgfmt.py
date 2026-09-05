"""服务端资料包格式校验（与 skill/scripts 同一规范，独立实现：服务器不信任任何输入）。"""

from __future__ import annotations

import hashlib
import io
import json
import re
import zipfile

import numpy as np

FORMAT_VERSION = 1
MAX_PACKAGE_BYTES = 200 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED = 2 * 1024 * 1024 * 1024
MAX_RATIO = 200
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
    top = {n.split("/", 1)[0] if "/" in n else n for n in names}
    for n in top:
        if n not in ALLOWED_TOP:
            raise PackageError(f"包含不允许的条目：{n}")
    total = 0
    for info in zf.infolist():
        name = info.filename
        if "\\" in name or name.startswith("/") or re.match(r"^[A-Za-z]:", name):
            raise PackageError(f"条目路径不合法：{name!r}")
        parts = name.split("/")
        if any(p == ".." for p in parts):
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
    if not isinstance(manifest.get("embed_dim"), int) or manifest["embed_dim"] <= 0:
        raise PackageError("embed_dim 必须是正整数")
    docs = manifest.get("documents")
    if not isinstance(docs, list) or not docs:
        raise PackageError("documents 必须是非空数组")

    seen: set[str] = set()
    expected_vec = 0
    for i, doc in enumerate(docs):
        where = f"documents[{i}]"
        h = doc.get("doc_hash", "")
        if not isinstance(doc, dict) or not HASH_RE.match(h):
            raise PackageError(f"{where}.doc_hash 不合法")
        if h in seen:
            raise PackageError(f"包内重复 doc_hash：{h[:12]}…")
        seen.add(h)
        if doc.get("doc_type") not in DOC_TYPES:
            raise PackageError(f"{where}.doc_type 不合法")
        for key in ("title", "original_filename", "department"):
            if not isinstance(doc.get(key), str) or not doc[key].strip():
                raise PackageError(f"{where}.{key} 必须是非空字符串")
        if doc.get("effective_date") is not None and not (
            isinstance(doc.get("effective_date"), str) and DATE_RE.match(doc["effective_date"])
        ):
            raise PackageError(f"{where}.effective_date 必须是 YYYY-MM-DD 或 null")
        if not isinstance(doc.get("audience"), list) or not all(
            isinstance(a, str) and a.strip() for a in doc["audience"]
        ):
            raise PackageError(f"{where}.audience 不合法")
        if not isinstance(doc.get("line_count"), int) or doc["line_count"] < 1:
            raise PackageError(f"{where}.line_count 不合法")
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
    if vec.get("dim") != manifest["embed_dim"]:
        raise PackageError("vectors.dim 与 embed_dim 不一致")
    return docs


def validate_package(
    data: bytes,
) -> tuple[dict, list[dict], "np.ndarray", dict[str, bytes], dict[str, str]]:
    """完整校验，返回 (manifest, documents, vectors, files{name:bytes}, texts{name:sha256…})。

    任何问题抛 PackageError。
    """
    if len(data) > MAX_PACKAGE_BYTES:
        raise PackageError(f"包大小超过上限 {MAX_PACKAGE_BYTES // (1024*1024)}MB")
    try:
        zf = zipfile.ZipFile(io.BytesIO(data))
    except zipfile.BadZipFile as exc:
        raise PackageError("不是有效的 zip 文件") from exc
    with zf:
        check_zip_safety(zf)
        names = set(zf.namelist())
        for required in ("manifest.json", "vectors.npy"):
            if required not in names:
                raise PackageError(f"缺少 {required}")
        try:
            manifest = json.loads(zf.read("manifest.json").decode("utf-8"))
        except Exception as exc:  # noqa: BLE001
            raise PackageError(f"manifest.json 解析失败：{exc}") from exc
        if not isinstance(manifest, dict):
            raise PackageError("manifest.json 顶层必须是对象")
        docs = _validate_manifest(manifest)

        vec_bytes = zf.read("vectors.npy")
        try:
            vecs = np.load(io.BytesIO(vec_bytes), allow_pickle=False)
        except Exception as exc:  # noqa: BLE001
            raise PackageError(f"vectors.npy 解析失败：{exc}") from exc
        if vecs.dtype != np.float32:
            raise PackageError("向量 dtype 必须是 float32")
        if vecs.shape != (manifest["vectors"]["count"], manifest["vectors"]["dim"]):
            raise PackageError("向量形状与 manifest 不一致")
        if not np.isfinite(vecs).all():
            raise PackageError("向量包含非有限值")

        files: dict[str, bytes] = {}
        texts: dict[str, str] = {}
        for doc in docs:
            h = doc["doc_hash"]
            ext = re.sub(r"[^a-z0-9.]", "", doc["original_filename"].lower().rsplit(".", 1)[-1])
            fname = f"files/{h}.{ext}"
            if fname not in names:
                raise PackageError(f"缺少原文件 {fname}")
            blob = zf.read(fname)
            if sha256_bytes(blob) != h:
                raise PackageError(f"原文件哈希不匹配：{fname}")
            files[fname] = blob
            tname = f"text/{h}.txt"
            if tname not in names:
                raise PackageError(f"缺少标准化原文 {tname}")
            tb = zf.read(tname)
            if sha256_bytes(tb) != doc["text_sha256"]:
                raise PackageError(f"标准化原文哈希不匹配：{tname}")
            lines = tb.decode("utf-8").splitlines()
            if len(lines) != doc["line_count"]:
                raise PackageError(f"{tname} 行数与 line_count 不一致")
            for ch in doc["chunks"]:
                if ch["text"] != "\n".join(lines[ch["line_start"] - 1 : ch["line_end"]]):
                    raise PackageError(f"{tname} chunk {ch['vector_index']} 与原文不一致")
                    break
            texts[tname] = tb.decode("utf-8")
    return manifest, docs, vecs, files, texts
