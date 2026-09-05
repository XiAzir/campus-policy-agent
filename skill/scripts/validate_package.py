"""第 4 步：资料包本地校验（与服务器端导入校验同一规范）。

用法：python skill/scripts/validate_package.py 资料包.zip
全部通过输出“校验通过”并以 0 退出；任何失败以 1 退出并逐条列出原因。
"""

from __future__ import annotations

import argparse
import io
import json
import sys
import zipfile
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import (  # noqa: E402
    PackageError,
    check_zip_safety,
    load_manifest,
    sha256_bytes,
    validate_manifest,
)

MAX_PACKAGE_BYTES = 200 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED = 2 * 1024 * 1024 * 1024
MAX_RATIO = 200


def validate(path: Path) -> list[str]:
    errors: list[str] = []
    data = path.read_bytes()
    if len(data) > MAX_PACKAGE_BYTES:
        return [f"包大小 {len(data)} 超过上限 {MAX_PACKAGE_BYTES}"]

    try:
        with zipfile.ZipFile(io.BytesIO(data)) as zf:
            try:
                check_zip_safety(zf, max_total_uncompressed=MAX_TOTAL_UNCOMPRESSED, max_ratio=MAX_RATIO)
            except PackageError as exc:
                return [str(exc)]

            names = set(zf.namelist())
            for required in ("manifest.json", "vectors.npy"):
                if required not in names:
                    return [f"缺少 {required}"]

            try:
                manifest = load_manifest(zf.read("manifest.json"))
                docs = validate_manifest(manifest)
            except PackageError as exc:
                return [str(exc)]

            vec_info = manifest["vectors"]
            vec_bytes = zf.read("vectors.npy")
            try:
                vecs = np.load(io.BytesIO(vec_bytes), allow_pickle=False)
            except Exception as exc:  # noqa: BLE001
                return [f"vectors.npy 解析失败：{exc}"]
            if vecs.dtype != np.float32:
                errors.append(f"向量 dtype 是 {vecs.dtype}，应为 float32")
            if vecs.shape != (vec_info["count"], vec_info["dim"]):
                errors.append(f"向量形状 {vecs.shape} 与 (count={vec_info['count']}, dim={vec_info['dim']}) 不一致")
            elif not np.isfinite(vecs).all():
                errors.append("向量包含非有限值")
            else:
                norms = np.linalg.norm(vecs, axis=1)
                if float(norms.min()) <= 1e-6 or not np.allclose(norms, 1, atol=1e-3):
                    errors.append("向量必须非零且已归一化")

            for doc in docs:
                h = doc["doc_hash"]
                ext = Path(doc["original_filename"]).suffix.lower()
                fname = f"files/{h}{ext}"
                if fname not in names:
                    errors.append(f"缺少原文件 {fname}")
                else:
                    if sha256_bytes(zf.read(fname)) != h:
                        errors.append(f"原文件哈希不匹配：{fname}")
                tname = f"text/{h}.txt"
                if tname not in names:
                    errors.append(f"缺少标准化原文 {tname}")
                    continue
                text_bytes = zf.read(tname)
                if sha256_bytes(text_bytes) != doc["text_sha256"]:
                    errors.append(f"标准化原文哈希不匹配：{tname}")
                lines = text_bytes.decode("utf-8").splitlines()
                if len(lines) != doc["line_count"]:
                    errors.append(
                        f"{tname} 实际 {len(lines)} 行，与 line_count={doc['line_count']} 不一致"
                    )
                for ch in doc["chunks"]:
                    expect = "\n".join(lines[ch["line_start"] - 1 : ch["line_end"]])
                    if ch["text"] != expect:
                        errors.append(f"{tname} chunk {ch['vector_index']} text 与原文行区间不一致")
                        break
    except zipfile.BadZipFile:
        return ["不是有效的 zip 文件"]
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description="资料包校验")
    parser.add_argument("package")
    args = parser.parse_args()
    path = Path(args.package)
    if not path.exists():
        print(f"文件不存在：{path}")
        return 1
    errors = validate(path)
    if errors:
        print("校验未通过：")
        for e in errors:
            print(f"  - {e}")
        return 1
    print("校验通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
