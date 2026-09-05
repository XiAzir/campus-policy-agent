"""中文分词与 FTS5 查询构造：写入侧与查询侧使用同一 jieba 分词器。"""

from __future__ import annotations

import re

import jieba

_FT_SAFE = re.compile(r'^[^\s"\'()*:{}]+$')


def tokenize_for_fts(text: str) -> str:
    """中文按 jieba 切词、空格分隔后写入 FTS5。"""
    return " ".join(t for t in jieba.cut_for_search(text) if t.strip())


def fts_match_query(text: str, max_tokens: int = 24) -> str:
    """查询串 → FTS5 MATCH 表达式（OR 语义，靠 bm25 排序）。

    纯符号/空白 token 丢弃；token 加双引号避免语法冲突。
    """
    tokens: list[str] = []
    for t in jieba.cut_for_search(text):
        t = t.strip()
        if t and _FT_SAFE.match(t) and t not in tokens:
            tokens.append(t)
        if len(tokens) >= max_tokens:
            break
    if not tokens:
        return ""
    return " OR ".join(f'"{t}"' for t in tokens)
