"""向量检索：每资料包一个 .npy（float32、已归一化），mmap 只读、分块扫描，不整包载入内存。

Windows 注意：mmap 句柄未释放时无法写/移动同名文件，因此写或替换向量文件前
必须调用对应路径的 close_path/close_all。同进程通过 get_index 共享单例。
"""

from __future__ import annotations

import gc
import json
from pathlib import Path

import numpy as np

BLOCK_ROWS = 4096
TOP_K_PER_BLOCK = 64


class VectorIndex:
    """管理 data/vectors/ 下各资料包的向量文件；按需 mmap，跨包扫描。"""

    def __init__(self, vectors_dir: Path):
        self.dir = Path(vectors_dir)
        self.dir.mkdir(parents=True, exist_ok=True)
        self._cache: dict[int, np.ndarray] = {}  # package_id → mmap 数组

    def _path(self, package_id: int) -> Path:
        return self.dir / f"pkg-{package_id}.npy"

    def _load(self, package_id: int) -> np.ndarray | None:
        if package_id in self._cache:
            return self._cache[package_id]
        path = self._path(package_id)
        if not path.exists():
            return None
        arr = np.load(path, mmap_mode="r", allow_pickle=False)
        if arr.ndim != 2:
            raise ValueError(f"向量文件形状异常：{path.name}")
        self._cache[package_id] = arr
        return arr

    def close_path(self, path: Path) -> None:
        """释放指向 path 的 mmap（写或替换该文件前调用）。"""
        key = str(Path(path).resolve()).lower()
        victims = [
            pid
            for pid, arr in self._cache.items()
            if str(Path(getattr(arr, "filename", "") or "").resolve()).lower() == key
        ]
        for pid in victims:
            del self._cache[pid]
        gc.collect()

    def close_all(self) -> None:
        """释放全部 mmap（备份恢复替换目录前调用）。"""
        self._cache.clear()
        gc.collect()

    def search(
        self,
        query_vec: np.ndarray,
        candidates: dict[int, list[int]],
        top_k: int,
    ) -> list[tuple[int, int]]:
        """在 candidates（package_id → row_index 列表）范围内扫描，返回 [(package_id, row), …]。

        距离用点积（向量已归一化即余弦）。每块 argpartition 取局部 top，最后全局合并。
        """
        q = np.asarray(query_vec, dtype=np.float32)
        norm = float(np.linalg.norm(q))
        if norm <= 0 or not np.isfinite(norm):
            return []
        q = q / norm

        best: list[tuple[float, int, int]] = []  # (-score, package_id, row)
        for pkg_id, rows in candidates.items():
            arr = self._load(pkg_id)
            if arr is None or arr.shape[1] != len(q):
                continue  # 维度不匹配的资料包不参与（须重建向量）
            rows_arr = np.asarray(sorted(rows), dtype=np.int64)
            rows_arr = rows_arr[rows_arr < arr.shape[0]]
            for i in range(0, rows_arr.size, BLOCK_ROWS):
                block_idx = rows_arr[i : i + BLOCK_ROWS]
                block = np.asarray(arr[block_idx], dtype=np.float32)  # 只物化当前块
                sims = block @ q
                k = min(TOP_K_PER_BLOCK, sims.size)
                part = np.argpartition(-sims, k - 1)[:k]
                for j in part:
                    best.append((-float(sims[j]), pkg_id, int(block_idx[j])))
        best.sort()
        return [(pkg, row) for _, pkg, row in best[:top_k]]


_index_singletons: dict[str, VectorIndex] = {}


def get_index(vectors_dir: Path) -> VectorIndex:
    """同进程同目录共享单例，保证 mmap 生命周期可集中管理。"""
    key = str(Path(vectors_dir).resolve()).lower()
    if key not in _index_singletons:
        _index_singletons[key] = VectorIndex(vectors_dir)
    return _index_singletons[key]


def close_mmap_for(vectors_dir: Path, path: Path) -> None:
    """写/移动向量文件前释放句柄：目录单例 + 文件路径。"""
    idx = _index_singletons.get(str(Path(vectors_dir).resolve()).lower())
    if idx:
        idx.close_path(path)


def vector_identity(embed_model: str, embed_dim: int, preprocessing_version: str) -> str:
    return json.dumps(
        {"model": embed_model, "dim": embed_dim, "pre": preprocessing_version},
        ensure_ascii=False,
        sort_keys=True,
    )
