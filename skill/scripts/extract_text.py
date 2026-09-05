"""第 1 步：提取标准化原文。

用法：python skill/scripts/extract_text.py -o work <原始文件...>
输出：work/standardized/<doc_hash>.txt 与 work/extract_report.json

- PDF（PyMuPDF）：每页前插入 `[[PDF页 n]]` 标记行；无文字层的文件标记退回，不做 OCR。
- DOCX（python-docx）：标题样式处插入 `[[SECTION 标题]]`；表格按行线性化为 `a | b | c`，不伪造分页。
- Markdown/文本：原样保留，`#`/`##`/`###` 作为章节标记。
- 只做空行折叠与行尾空白清理，不改写任何原文文字。
- page_map 与章节区间在清理后的成品文本上重新扫描标记得出，保证与最终行号一致。
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from common import sha256_file  # noqa: E402

PAGE_RE = re.compile(r"^\[\[PDF页 (\d+)\]\]$")
SECTION_RE = re.compile(r"^\[\[SECTION (.+)\]\]$")
MIN_CHARS_PER_PAGE = 20  # 平均每页可提取字符低于此值视为无文字层


def clean_block(lines: list[str]) -> list[str]:
    """去掉行尾空白；连续空行折叠为一个空行。"""
    out: list[str] = []
    prev_blank = False
    for line in lines:
        line = line.rstrip()
        blank = line == ""
        if blank and prev_blank:
            continue
        out.append(line)
        prev_blank = blank
    while out and out[0] == "":
        out.pop(0)
    while out and out[-1] == "":
        out.pop()
    return out


def scan_sections(lines: list[str]) -> list[dict]:
    marks = [
        (i + 1, SECTION_RE.match(line).group(1).strip())
        for i, line in enumerate(lines)
        if SECTION_RE.match(line)
    ]
    sections = []
    for i, (line_no, title) in enumerate(marks):
        start = line_no + 1
        end = marks[i + 1][0] - 1 if i + 1 < len(marks) else len(lines)
        sections.append(
            {"section_id": f"s{i+1}", "title": title, "start_line": start, "end_line": max(start - 1, end)}
        )
    return sections


def extract_pdf(path: Path) -> tuple[list[str], list[str]]:
    try:
        import pymupdf as fitz
    except ImportError:
        import fitz  # 旧版 pymupdf

    doc = fitz.open(path)
    lines: list[str] = []
    warnings: list[str] = []
    empty_pages = 0
    total_chars = 0
    page_count = doc.page_count
    for page_no in range(1, page_count + 1):
        text = doc[page_no - 1].get_text("text")
        block = text.splitlines()
        n_chars = sum(len(l.strip()) for l in block)
        total_chars += n_chars
        if n_chars == 0:
            empty_pages += 1
        lines.append(f"[[PDF页 {page_no}]]")
        lines.extend(block)
    doc.close()
    if total_chars < MIN_CHARS_PER_PAGE * max(1, page_count):
        return [], [f"无文字层（扫描件），共提取 {total_chars} 字符，不做 OCR，退回处理"]
    if empty_pages:
        warnings.append(f"{empty_pages} 页无可提取文字（可能是图片页）")
    return clean_block(lines), warnings


def extract_docx(path: Path) -> tuple[list[str], list[str]]:
    from docx import Document
    from docx.table import Table
    from docx.text.paragraph import Paragraph

    doc = Document(path)
    lines: list[str] = []
    warnings: list[str] = []

    # iter_inner_content 按文档顺序给出 Paragraph/Table 包装对象（python-docx>=1.1）
    for child in doc.iter_inner_content():
        if isinstance(child, Paragraph):
            style = (child.style.name or "").lower() if child.style is not None else ""
            text = child.text.strip()
            if re.match(r"^heading\s*[1-9]", style) and text:
                lines.append(f"[[SECTION {text}]]")
            elif text:
                lines.append(child.text.rstrip())
        elif isinstance(child, Table):
            lines.append("[[表格]]")
            for row in child.rows:
                lines.append(" | ".join(cell.text.strip().replace("\n", " ") for cell in row.cells))
            lines.append("[[/表格]]")
    return clean_block(lines), warnings


def extract_textfile(path: Path) -> tuple[list[str], list[str]]:
    raw = path.read_text(encoding="utf-8", errors="replace")
    lines: list[str] = []
    for line in raw.splitlines():
        m = re.match(r"^(#{1,3})\s+(.*)$", line)
        lines.append(f"[[SECTION {m.group(2).strip()}]]" if m else line.rstrip())
    return clean_block(lines), []


def main() -> int:
    parser = argparse.ArgumentParser(description="提取标准化原文")
    parser.add_argument("-o", "--out", default="work", help="工作目录")
    parser.add_argument("files", nargs="+", help="原始文件（pdf/docx/md/txt）")
    args = parser.parse_args()

    out_dir = Path(args.out)
    std_dir = out_dir / "standardized"
    std_dir.mkdir(parents=True, exist_ok=True)

    report = {"documents": []}
    failed = False
    for raw_path in args.files:
        path = Path(raw_path)
        ext = path.suffix.lower().lstrip(".")
        if ext not in {"pdf", "docx", "md", "txt"}:
            hint = "请另存为 .docx 或 PDF" if ext == "doc" else f"不支持的类型 .{ext}"
            report["documents"].append({"original_filename": path.name, "error": hint})
            failed = True
            print(f"[失败] {path.name}: {hint}")
            continue

        doc_hash = sha256_file(path)
        if ext == "pdf":
            lines, warnings = extract_pdf(path)
        elif ext == "docx":
            lines, warnings = extract_docx(path)
        else:
            lines, warnings = extract_textfile(path)

        if not lines:
            report["documents"].append(
                {
                    "original_filename": path.name,
                    "doc_hash": doc_hash,
                    "doc_type": ext,
                    "needs_ocr": True,
                    "warnings": warnings,
                }
            )
            failed = True
            print(f"[退回] {path.name}: {warnings[0] if warnings else '无可提取文字'}")
            continue

        text_path = std_dir / f"{doc_hash}.txt"
        text_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

        page_map = None
        if ext == "pdf":
            page_map = [[1, 1]]
            for i, line in enumerate(lines, start=1):
                m = PAGE_RE.match(line)
                if m and int(m.group(1)) > 1:
                    page_map.append([i, int(m.group(1))])

        entry = {
            "original_filename": path.name,
            "doc_hash": doc_hash,
            "doc_type": ext,
            "needs_ocr": False,
            "warnings": warnings,
            "line_count": len(lines),
            "text_file": str(text_path),
            "page_map": page_map,
            "sections": scan_sections(lines),
        }
        report["documents"].append(entry)
        extra = f"，{len(page_map)} 页" if page_map else ""
        print(f"[完成] {path.name} → {doc_hash[:12]}… 共 {len(lines)} 行{extra}")

    (out_dir / "extract_report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    if failed:
        print("\n存在退回或失败文件，见 extract_report.json；处理后重跑。")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
