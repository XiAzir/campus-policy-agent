"""Bounded file operations shared by package imports and backup recovery."""

import hashlib
import shutil
from pathlib import Path

from .config import config

BLOCK_BYTES = 1024 * 1024
RESERVE_BYTES = 128 * 1024 * 1024


def hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(BLOCK_BYTES), b""):
            digest.update(block)
    return digest.hexdigest()


def require_space(path, needed):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(path).free < needed + RESERVE_BYTES:
        raise ValueError("磁盘空间不足：无法容纳临时文件、索引与回滚余量")


def remove_tree(parent, target):
    parent, target = Path(parent).resolve(), Path(target).resolve()
    if target == parent or not target.is_relative_to(parent):
        raise ValueError("拒绝清理工作目录之外的路径")
    if target.exists():
        shutil.rmtree(target)
