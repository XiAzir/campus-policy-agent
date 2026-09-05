"""第 3 步：构建资料包。

用法：python skill/scripts/build_package.py --work work --out 资料包-xxx.zip [--base-url URL]

需要 work/standardized/*.txt、work/extract_report.json、work/meta.json（第 2 步产物）。
向量经硅基流动 API 在本地生成（密钥从仓库根 .env 读取），已 L2 归一化后写入 vectors.npy。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import zipfile
from pathlib import Path

import httpx
import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import FORMAT_VERSION, sha256_bytes, sha256_file, chunk_lines  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
EMBED_BATCH = 16
RETRIES = 3


def load_env() -> None:
    env_path = REPO / ".env"
    if env_path.exists():
        for line in env_path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                os.environ.setdefault(k.strip(), v.strip())
    for key in ("SILICONFLOW_API_KEY", "SILICONFLOW_EMBED_MODEL"):
        if not os.environ.get(key):
            raise SystemExit(f"缺少环境变量 {key}（应在 .env 中配置，不得写入资料包）")


def embed_texts(texts: list[str], dims: int) -> np.ndarray:
    """调用硅基流动 embeddings，返回 (N, dims) 归一化 float32 矩阵。"""
    url = "https://api.siliconflow.cn/v1/embeddings"
    headers = {"Authorization": f"Bearer {os.environ['SILICONFLOW_API_KEY']}"}
    out = np.zeros((len(texts), dims), dtype=np.float32)
    done = 0
    t0 = time.time()
    while done < len(texts):
        batch = texts[done : done + EMBED_BATCH]
        payload = {"model": os.environ["SILICONFLOW_EMBED_MODEL"], "input": batch, "dimensions": dims}
        last_err = ""
        for attempt in range(1, RETRIES + 1):
            try:
                r = httpx.post(url, headers=headers, json=payload, timeout=120)
                if r.status_code == 200:
                    data = r.json()["data"]
                    data.sort(key=lambda x: x["index"])
                    vecs = np.asarray([d["embedding"] for d in data], dtype=np.float32)
                    if vecs.shape[1] != dims:
                        raise RuntimeError(f"返回维度 {vecs.shape[1]} 与期望 {dims} 不一致")
                    norms = np.linalg.norm(vecs, axis=1, keepdims=True)
                    if float(norms.min()) <= 0:
                        raise RuntimeError("出现零向量")
                    out[done : done + len(batch)] = vecs / norms
                    done += len(batch)
                    print(f"  向量 {done}/{len(texts)}（{time.time()-t0:.0f}s）")
                    last_err = ""
                    break
                last_err = f"HTTP {r.status_code}: {r.text[:200]}"
            except Exception as exc:  # noqa: BLE001
                last_err = f"{type(exc).__name__}: {exc}"
            if attempt < RETRIES:
                time.sleep(2 * attempt)
        if last_err:
            raise SystemExit(f"embedding 失败（已重试 {RETRIES} 次）：{last_err}")
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description="构建资料包")
    parser.add_argument("--work", default="work")
    parser.add_argument("--out", required=True)
    parser.add_argument("--materials", default="", help="原始文件所在目录（默认依次尝试 work 上级、当前目录、仓库根）")
    parser.add_argument("--dims", type=int, default=int(os.environ.get("EMBED_DIMS", "1024")))
    args = parser.parse_args()

    load_env()
    work = Path(args.work)
    report = json.loads((work / "extract_report.json").read_text(encoding="utf-8"))
    meta = json.loads((work / "meta.json").read_text(encoding="utf-8"))
    meta_by_hash = {d["doc_hash"]: d for d in meta["documents"]}

    out_path = Path(args.out)
    if out_path.suffix.lower() != ".zip":
        out_path = out_path.with_suffix(".zip")
    tmp_dir = work / "package"
    if tmp_dir.exists():
        import shutil

        shutil.rmtree(tmp_dir)
    (tmp_dir / "files").mkdir(parents=True)
    (tmp_dir / "text").mkdir(parents=True)

    manifest_docs: list[dict] = []
    all_texts: list[str] = []
    vec_index = 0

    for entry in report["documents"]:
        if entry.get("error") or entry.get("needs_ocr"):
            raise SystemExit(
                f"{entry['original_filename']} 存在未处理问题（error/needs_ocr），先解决再打包"
            )
        h = entry["doc_hash"]
        m = meta_by_hash.get(h)
        if m is None:
            raise SystemExit(f"meta.json 缺少 doc_hash={h[:12]}… 的元数据（{entry['original_filename']}）")
        original = None
        candidates = [Path(args.work).parent, Path.cwd(), REPO]
        if args.materials:
            candidates.insert(0, Path(args.materials))
        for cand in candidates:
            p = cand / entry["original_filename"]
            if p.exists() and sha256_file(p) == h:
                original = p
                break
        if original is None:
            raise SystemExit(
                f"找不到 {entry['original_filename']} 的原始文件（且哈希不匹配），请用 --materials 指定其所在目录"
            )

        text_lines = (work / "standardized" / f"{h}.txt").read_text(encoding="utf-8").splitlines()
        # meta.json 可用 "sections" 覆盖自动检测的章节（如"一、二、三"式标题）
        sections = m.get("sections") or entry["sections"]
        raw_chunks = chunk_lines(text_lines, sections)
        for ch in raw_chunks:
            ch["vector_index"] = vec_index
            vec_index += 1
            ch["section_id"] = next(
                (
                    s["section_id"]
                    for s in sections
                    if s["start_line"] <= ch["line_start"] and ch["line_end"] <= s["end_line"]
                ),
                None,
            )
            all_texts.append(ch["text"])

        ext = Path(entry["original_filename"]).suffix.lower()
        (tmp_dir / "files" / f"{h}{ext}").write_bytes(original.read_bytes())
        text_src = work / "standardized" / f"{h}.txt"
        (tmp_dir / "text" / f"{h}.txt").write_bytes(text_src.read_bytes())
        text_sha = sha256_file(text_src)

        manifest_docs.append(
            {
                "doc_hash": h,
                "original_filename": entry["original_filename"],
                "doc_type": entry["doc_type"],
                "title": m["title"],
                "department": m["department"],
                "effective_date": m.get("effective_date"),
                "audience": m.get("audience", []),
                "audience_scope": m.get("audience_scope", {}),
                "domains": m.get("domains", []),
                "replaces": m.get("replaces"),
                "notes": m.get("notes", ""),
                "line_count": entry["line_count"],
                "page_map": entry["page_map"],
                "sections": sections,
                "text_sha256": text_sha,
                "chunks": raw_chunks,
            }
        )

    print(f"共 {vec_index} 个分块，开始生成向量（{args.dims} 维）…")
    vectors = embed_texts(all_texts, args.dims)
    np.save(tmp_dir / "vectors.npy", vectors)

    manifest = {
        "format_version": FORMAT_VERSION,
        "preprocessing_version": meta.get("preprocessing_version", "v1"),
        "embed_model": os.environ["SILICONFLOW_EMBED_MODEL"],
        "embed_dim": args.dims,
        "documents": manifest_docs,
        "vectors": {"file": "vectors.npy", "dtype": "float32", "count": vec_index, "dim": args.dims},
    }
    (tmp_dir / "manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )

    with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as zf:
        for p in sorted(tmp_dir.rglob("*")):
            if p.is_file():
                zf.write(p, p.relative_to(tmp_dir).as_posix())

    print(f"资料包已生成：{out_path}（{out_path.stat().st_size/1e6:.1f} MB，{len(manifest_docs)} 份资料，{vec_index} 个分块）")
    print("下一步：python skill/scripts/validate_package.py", out_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
