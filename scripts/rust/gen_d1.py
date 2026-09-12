"""D1/D3 合成数据集生成器（spec 3.2 节）。

在测试机运行，产物写入忽略目录（默认 .local-acceptance/datasets/），不入 git。
- D1：固定种子，100 份文档、10,000 分块、1024 维 float32，按资料包上限拆分为多个合法包。
- D3：100,000 分块（约 390.6 MiB 裸向量），仅用于压力上限探索。

向量要求：有限、归一化、方向多样、相似度间距可控——用正交基块 + 受控扰动，
不做全零/全重复等易压缩数据。文档正文为真实长度分布的合成中文。
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import sys
import zipfile
from pathlib import Path

import numpy as np

REPO = Path(__file__).resolve().parents[2]
DEFAULT_OUT = REPO / ".local-acceptance" / "datasets"

WORDS = [
    "学生", "管理", "办法", "规定", "申请", "审批", "处分", "申诉", "奖学金", "助学金",
    "宿舍", "安全", "实践", "报名", "截止", "学院", "教师", "课程", "学分", "绩点",
    "图书馆", "实验室", "考勤", "请假", "违纪", "警告", "严重警告", "记过", "留校察看", "开除学籍",
    "社会实践", "三下乡", "创新创业", "训练计划", "项目", "立项", "结题", "经费", "报销", "发票",
    "学籍", "注册", "转专业", "休学", "复学", "退学", "毕业", "学位", "论文", "答辩",
]
SENTENCES = [
    "第{n}条 {a}应当在{b}前向{c}提交书面申请，逾期不予受理。",
    "第{n}条 {a}违反本规定{b}的，视情节轻重给予{c}处分。",
    "第{n}条 {c}负责解释本办法，并可制定实施细则。",
    "第{n}条 本办法自发布之日起施行，适用于{a}和{b}。",
    "第{n}条 {b}的认定标准由{c}另行通知，{a}须配合核查。",
    "第{n}条 对{b}结果有异议的，{a}可在十个工作日内向{c}提出申诉。",
]


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def synth_text(doc_index: int, chunk_count: int, rng: np.random.Generator) -> tuple[str, list[str], list[int]]:
    """生成 1 份文档正文与分块：每块 8~14 行，行长真实分布。"""
    lines_per_chunk = []
    chunks_text = []
    line_no = 1
    all_lines = [f"测试资料第{doc_index}号：XX大学学生管理办法（合成样本 {doc_index:03d}）"]
    for ci in range(chunk_count):
        n = int(rng.integers(8, 15))
        block = []
        for _ in range(n):
            tmpl = SENTENCES[int(rng.integers(0, len(SENTENCES)))]
            block.append(tmpl.format(
                n=line_no,
                a=WORDS[int(rng.integers(0, len(WORDS)))],
                b=WORDS[int(rng.integers(0, len(WORDS)))],
                c=WORDS[int(rng.integers(0, len(WORDS)))] + "处",
            ))
            line_no += 1
        all_lines.extend(block)
        lines_per_chunk.append(n)
        chunks_text.append("\n".join(block))
    return "\n".join(all_lines) + "\n", chunks_text, lines_per_chunk


def make_vectors(count: int, dim: int, rng: np.random.Generator) -> np.ndarray:
    """正交基块 + 受控扰动的归一化向量：方向多样、间距可控、非退化。

    dim>=count 时直接取正交基的前 count 行；否则分块正交并叠加 0.05 幅度扰动
    后归一化——保证有限、非零、范数 1，且不出现全重复行。
    """
    if count <= dim:
        basis = np.eye(dim, dtype=np.float32)[:count]
    else:
        reps = int(np.ceil(count / dim))
        basis = np.tile(np.eye(dim, dtype=np.float32), (reps, 1))[:count]
    noise = rng.standard_normal(basis.shape).astype(np.float32) * 0.05
    mat = basis + noise
    mat /= np.linalg.norm(mat, axis=1, keepdims=True)
    return mat.astype(np.float32)


def build_packages(total_docs: int, chunks_per_doc: int, dim: int, seed: int,
                   max_docs_per_pkg: int = 20) -> list[tuple[bytes, dict]]:
    """按 max_docs_per_pkg 拆分为多个 format_version=1 合法包。"""
    rng = np.random.default_rng(seed)
    packages = []
    doc_idx = 0
    while doc_idx < total_docs:
        n_docs = min(max_docs_per_pkg, total_docs - doc_idx)
        documents, entries, vec_rows = [], {}, []
        for k in range(n_docs):
            text, chunks_text, lines_per_chunk = synth_text(doc_idx, chunks_per_doc, rng)
            blob = text.encode("utf-8")
            h = sha256_bytes(blob)
            sections = []
            start = 2  # Line 1 is the document title, not part of a chunk.
            for ci, n in enumerate(lines_per_chunk):
                sections.append({"section_id": f"s{ci}", "title": f"第{ci + 1}节",
                                 "start_line": start, "end_line": start + n - 1})
                start += n
            documents.append({
                "doc_hash": h, "original_filename": f"synthetic-{doc_idx:04d}.txt", "doc_type": "txt",
                "title": f"合成学生管理办法第{doc_idx}号", "department": "测试部",
                "effective_date": "2026-01-01", "audience": ["全校"], "audience_scope": {},
                "domains": [{"tag": "学生事务" if doc_idx % 2 == 0 else "安全纪律", "section_ids": []}],
                "replaces": None, "notes": "", "line_count": text.count("\n"), "page_map": None,
                "sections": sections, "text_sha256": h,
                "chunks": [{"chunk_id": ci, "vector_index": len(vec_rows) + ci, "text": t,
                            "line_start": s["start_line"], "line_end": s["end_line"],
                            "section_id": s["section_id"]}
                           for ci, (t, s) in enumerate(zip(chunks_text, sections))],
            })
            entries[f"files/{h}.txt"] = blob
            entries[f"text/{h}.txt"] = blob
            vec_rows.extend(documents[-1]["chunks"])
            doc_idx += 1
        mat = make_vectors(len(vec_rows), dim, rng)
        buf = io.BytesIO()
        np.save(buf, mat)
        entries["vectors.npy"] = buf.getvalue()
        manifest = {
            "format_version": 1, "preprocessing_version": "v1", "embed_model": "test-embed",
            "embed_dim": dim, "documents": documents,
            "vectors": {"file": "vectors.npy", "dtype": "float32",
                        "count": len(vec_rows), "dim": dim},
        }
        entries["manifest.json"] = json.dumps(manifest, ensure_ascii=False).encode()
        out = io.BytesIO()
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
            for name, data in entries.items():
                zf.writestr(name, data)
        packages.append((out.getvalue(), manifest))
    return packages


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", choices=["d1", "d3"], default="d1")
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = ap.parse_args()

    if args.dataset == "d1":
        total_docs, chunks_per_doc, dim, seed = 100, 100, 1024, 20260906
    else:
        total_docs, chunks_per_doc, dim, seed = 1000, 100, 1024, 20260907

    out_dir = args.out / args.dataset
    out_dir.mkdir(parents=True, exist_ok=True)
    packages = build_packages(total_docs, chunks_per_doc, dim, seed)
    total_chunks = 0
    total_bytes = 0
    for i, (blob, manifest) in enumerate(packages, 1):
        p = out_dir / f"d1-pkg-{i:03d}.zip" if args.dataset == "d1" else f"d3-pkg-{i:03d}.zip"
        p.write_bytes(blob)
        total_chunks += manifest["vectors"]["count"]
        total_bytes += len(blob)
        print(f"{p.name}: {len(blob) / 1e6:.1f} MB, docs={len(manifest['documents'])}, "
              f"chunks={manifest['vectors']['count']}")
    print(f"SUMMARY {args.dataset}: packages={len(packages)} docs={total_docs} "
          f"chunks={total_chunks} dim={dim} zip_total={total_bytes / 1e6:.1f}MB "
          f"raw_vectors={total_chunks * dim * 4 / 1024 / 1024:.1f}MiB")


if __name__ == "__main__":
    main()
