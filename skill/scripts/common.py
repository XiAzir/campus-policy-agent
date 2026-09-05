"""资料包预处理共用逻辑：哈希、分块、manifest 校验规则。

此模块是资料包格式（docs/资料包格式说明.md）的规范化实现；
服务器端导入校验独立实现同一规范，两端以格式文档为准。
"""

from __future__ import annotations

import hashlib
import json
import re
import zipfile
from pathlib import Path

FORMAT_VERSION = 1
CHUNK_TARGET_CHARS = 450
CHUNK_MIN_CHARS = 200
CHUNK_HARD_MAX = 800

ALLOWED_TOP = {"manifest.json", "vectors.npy", "files", "text"}
ALLOWED_PREFIXES = ("files/", "text/")

DOC_TYPES = {"pdf", "docx", "md", "txt"}
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
HASH_RE = re.compile(r"^[0-9a-f]{64}$")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def chunk_lines(lines: list[str], sections: list[dict] | None = None) -> list[dict]:
    """按空行分段，贪心合并段落到目标长度；chunk.text 与原文行区间逐字符一致。"""
    if sections:
        boundaries = sorted({1, len(lines) + 1, *(s["start_line"] for s in sections), *(s["end_line"] + 1 for s in sections)})
        result = []
        for start, end in zip(boundaries, boundaries[1:]):
            for ch in chunk_lines(lines[start - 1:end - 1]):
                ch["line_start"] += start - 1
                ch["line_end"] += start - 1
                result.append(ch)
        return result
    paragraphs: list[tuple[int, int]] = []
    start = None
    has_content = False
    for i, line in enumerate(lines, start=1):
        if line.strip() == "":
            if has_content:
                paragraphs.append((start, i - 1))
                start, has_content = None, False
        else:
            if start is None:
                start = i
            has_content = True
    if has_content:
        paragraphs.append((start, len(lines)))

    ranges: list[tuple[int, int]] = []
    cur: list[tuple[int, int]] = []
    cur_chars = 0

    def flush() -> None:
        nonlocal cur, cur_chars
        if cur:
            ranges.append((cur[0][0], cur[-1][1]))
            cur, cur_chars = [], 0

    for (s, e) in paragraphs:
        plen = sum(len(lines[i - 1]) for i in range(s, e + 1))
        if plen > CHUNK_HARD_MAX:
            flush()
            block_start, chars = s, 0
            for line_no in range(s, e + 1):
                size = len(lines[line_no - 1])
                if size > 4000:
                    raise ValueError("单行原文超过 4000 字符，请管理员确认并在提取阶段按原文段落分行")
                if chars and chars + size > CHUNK_HARD_MAX:
                    ranges.append((block_start, line_no - 1))
                    block_start, chars = line_no, 0
                chars += size
            ranges.append((block_start, e))
            continue
        if cur and cur_chars + plen > CHUNK_TARGET_CHARS and cur_chars >= CHUNK_MIN_CHARS:
            flush()
        cur.append((s, e))
        cur_chars += plen
        if cur_chars >= CHUNK_TARGET_CHARS:
            flush()
    flush()

    return [
        {"text": "\n".join(lines[s - 1 : e]), "line_start": s, "line_end": e}
        for (s, e) in ranges
    ]


class PackageError(Exception):
    """校验失败，message 面向管理员。"""


def check_zip_safety(zf: zipfile.ZipFile, *, max_total_uncompressed: int, max_ratio: float) -> int:
    """白名单条目 + 路径穿越 + 符号链接 + 解压规模检查，返回解压总字节数。"""
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
    if total > max_total_uncompressed:
        raise PackageError(f"解压总量 {total} 超过上限 {max_total_uncompressed}")
    compressed = sum(i.compress_size for i in zf.infolist()) or 1
    if total / compressed > max_ratio:
        raise PackageError(f"压缩比异常（疑似解压炸弹）：{total / compressed:.0f}")
    return total


def load_manifest(data: bytes) -> dict:
    try:
        manifest = json.loads(data.decode("utf-8"))
    except Exception as exc:  # noqa: BLE001
        raise PackageError(f"manifest.json 解析失败：{exc}") from exc
    if not isinstance(manifest, dict):
        raise PackageError("manifest.json 顶层必须是对象")
    return manifest


def validate_manifest(manifest: dict) -> list[dict]:
    """结构校验，返回 documents 列表（含校验过的字段）。跨文件一致性（哈希/向量）由调用方检查。"""
    if manifest.get("format_version") != FORMAT_VERSION:
        raise PackageError(f"format_version 必须是 {FORMAT_VERSION}")
    if not isinstance(manifest.get("preprocessing_version"), str) or not manifest["preprocessing_version"]:
        raise PackageError("preprocessing_version 缺失")
    for key in ("embed_model", "embed_dim"):
        if key not in manifest:
            raise PackageError(f"{key} 缺失")
    if not isinstance(manifest["embed_dim"], int) or manifest["embed_dim"] <= 0:
        raise PackageError("embed_dim 必须是正整数")
    docs = manifest.get("documents")
    if not isinstance(docs, list) or not docs:
        raise PackageError("documents 必须是非空数组")

    seen_hashes: set[str] = set()
    seen_sections: dict[str, set[str]] = {}
    vector_index_seen: set[int] = set()
    expected_vec_count = 0

    for i, doc in enumerate(docs):
        where = f"documents[{i}]"
        if not isinstance(doc, dict):
            raise PackageError(f"{where} 必须是对象")
        doc_hash = doc.get("doc_hash", "")
        if not HASH_RE.match(doc_hash):
            raise PackageError(f"{where}.doc_hash 不是 64 位十六进制")
        if doc_hash in seen_hashes:
            raise PackageError(f"包内重复 doc_hash：{doc_hash[:12]}…")
        seen_hashes.add(doc_hash)
        if doc.get("doc_type") not in DOC_TYPES:
            raise PackageError(f"{where}.doc_type 必须是 {'/'.join(sorted(DOC_TYPES))}")
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
            raise PackageError(f"{where}.audience 必须是非空字符串数组")
        if not isinstance(doc.get("line_count"), int) or doc["line_count"] < 1:
            raise PackageError(f"{where}.line_count 必须是正整数")
        if doc["doc_type"] == "pdf":
            pm = doc.get("page_map")
            if not isinstance(pm, list) or not pm or pm[0] != [1, 1]:
                raise PackageError(f"{where}.page_map（PDF）必须以 [1,1] 开头")
            last_line = 0
            last_page = 0
            for pair in pm:
                if (
                    not isinstance(pair, list)
                    or len(pair) != 2
                    or not all(isinstance(x, int) for x in pair)
                    or pair[0] <= last_line
                    or pair[1] < last_page
                ):
                    raise PackageError(f"{where}.page_map 区段必须按行号升序且页码不减")
                last_line, last_page = pair
        if not isinstance(doc.get("text_sha256"), str) or not HASH_RE.match(doc["text_sha256"]):
            raise PackageError(f"{where}.text_sha256 不合法")

        section_ids = set()
        for sec in doc.get("sections", []):
            if not isinstance(sec, dict) or not isinstance(sec.get("section_id"), str) or not sec["section_id"]:
                raise PackageError(f"{where}.sections.section_id 缺失")
            if sec["section_id"] in section_ids:
                raise PackageError(f"{where} 章节重复：{sec['section_id']}")
            section_ids.add(sec["section_id"])
            if not (1 <= sec.get("start_line", 0) <= sec.get("end_line", 0) <= doc["line_count"]):
                raise PackageError(f"{where} 章节 {sec['section_id']} 行区间越界")
        seen_sections[doc_hash] = section_ids

        for dom in doc.get("domains", []):
            if not isinstance(dom, dict) or not isinstance(dom.get("tag"), str) or not dom["tag"].strip():
                raise PackageError(f"{where}.domains.tag 必须是非空字符串")
            for sid in dom.get("section_ids", []):
                if sid not in section_ids:
                    raise PackageError(f"{where} 领域 {dom['tag']} 引用了不存在的章节 {sid}")

        chunks = doc.get("chunks")
        if not isinstance(chunks, list) or not chunks:
            raise PackageError(f"{where}.chunks 必须是非空数组")
        prev_end = 0
        for ch in chunks:
            vi = ch.get("vector_index")
            if not isinstance(vi, int) or vi in vector_index_seen or vi != expected_vec_count:
                raise PackageError(f"{where} chunk vector_index 必须从 0 开始连续")
            vector_index_seen.add(vi)
            expected_vec_count += 1
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
            ch["_doc"] = doc  # 供调用方按文档分组

    vec = manifest.get("vectors")
    if not isinstance(vec, dict) or vec.get("file") != "vectors.npy":
        raise PackageError("vectors.file 必须是 vectors.npy")
    if vec.get("dtype") != "float32":
        raise PackageError("vectors.dtype 必须是 float32")
    if vec.get("count") != expected_vec_count:
        raise PackageError(f"vectors.count {vec.get('count')} 与 chunk 总数 {expected_vec_count} 不一致")
    if vec.get("dim") != manifest["embed_dim"]:
        raise PackageError("vectors.dim 与 embed_dim 不一致")
    return docs
