#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Convert the authoritative Word spec into Markdown, preserving document order.

The converter walks the document body (not just ``document.paragraphs``), so
paragraphs and tables stay interleaved exactly as they appear in Word. It is
re-runnable: the output file is fully rewritten from the .docx on every run.

Run with the DSH runtime Python (python-docx is preinstalled):

    $env:PYTHONIOENCODING='utf-8'
    & "C:\\Users\\firmm\\.dsh\\dsh-runtimes\\dsh-primary-runtime\\dependencies\\python\\python.exe" `
        scripts/extract_spec.py

Options:
    --docx PATH   source .docx (default: Image_Toolbox_开发规格书_v1.0.docx)
    --out PATH    output Markdown (default: docs/spec.md)
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import Iterator, Union

from docx import Document
from docx.document import Document as _Document
from docx.oxml.ns import qn
from docx.table import Table
from docx.text.paragraph import Paragraph

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DOCX = REPO_ROOT / "Image_Toolbox_开发规格书_v1.0.docx"
DEFAULT_OUT = REPO_ROOT / "docs" / "spec.md"

# Word built-in style names -> Markdown heading level.
# Title is the document title (#), Heading 1 is a section (##), and so on.
TITLE_LEVEL = 1
HEADING_LEVELS = {
    "Heading 1": 2,
    "Heading 2": 3,
    "Heading 3": 4,
    "Heading 4": 5,
    "Heading 5": 6,
    "Heading 6": 7,
}

Block = Union[Paragraph, Table]


def iter_block_items(doc: _Document) -> Iterator[Block]:
    """Yield body paragraphs and tables in document order."""
    body = doc.element.body
    for child in body.iterchildren():
        if child.tag == qn("w:p"):
            yield Paragraph(child, doc)
        elif child.tag == qn("w:tbl"):
            yield Table(child, doc)


def cell_text(text: str) -> str:
    """Make one table cell safe for a Markdown table."""
    return text.replace("|", "\\|").replace("\r\n", "\n").replace("\n", "<br>").strip()


def render_table(table: Table, number: int) -> list[str]:
    rows = []
    for row in table.rows:
        cells = [cell_text(cell.text) for cell in row.cells]
        rows.append(cells)
    if not rows:
        return []

    width = max(len(row) for row in rows)
    rows = [row + [""] * (width - len(row)) for row in rows]
    header, body = rows[0], rows[1:]

    lines = ["<!-- table {} -->".format(number), ""]
    lines.append("| " + " | ".join(header) + " |")
    lines.append("| " + " | ".join(["---"] * width) + " |")
    for row in body:
        lines.append("| " + " | ".join(row) + " |")
    lines.append("")
    return lines


def fence_for(lines: list[str]) -> str:
    longest = 0
    for line in lines:
        run = 0
        for ch in line:
            if ch == "`":
                run += 1
                longest = max(longest, run)
            else:
                run = 0
    return "`" * max(3, longest + 1)


def is_code(paragraph: Paragraph) -> bool:
    # The spec uses a custom "CodeText" paragraph style for preformatted lines.
    return paragraph.style is not None and paragraph.style.name == "CodeText"


def convert(docx_path: Path) -> tuple[str, dict[str, int]]:
    doc = Document(str(docx_path))
    blocks = list(iter_block_items(doc))

    out: list[str] = [
        "> 本文件由 `scripts/extract_spec.py` 从 "
        "`Image_Toolbox_开发规格书_v1.0.docx` 自动转换，"
        "保留原文顺序与全部表格，未增删规格内容。",
        "",
    ]

    stats = {"paragraphs": 0, "tables": 0, "code_blocks": 0, "headings": 0}
    list_number = 0
    index = 0

    while index < len(blocks):
        block = blocks[index]

        if isinstance(block, Table):
            stats["tables"] += 1
            out.extend(render_table(block, stats["tables"]))
            list_number = 0
            index += 1
            continue

        paragraph: Paragraph = block
        text = paragraph.text.strip()
        style = paragraph.style.name if paragraph.style is not None else "Normal"

        # Fenced code: merge consecutive CodeText paragraphs, including blank lines.
        if is_code(paragraph):
            code_lines: list[str] = []
            while index < len(blocks) and isinstance(blocks[index], Paragraph) and is_code(blocks[index]):
                code_lines.append(blocks[index].text.rstrip())
                stats["paragraphs"] += 1
                index += 1
            while code_lines and code_lines[-1] == "":
                code_lines.pop()
            fence = fence_for(code_lines)
            out.append(fence)
            out.extend(code_lines)
            out.append(fence)
            out.append("")
            stats["code_blocks"] += 1
            list_number = 0
            continue

        stats["paragraphs"] += 1

        if style == "Title":
            out.extend(["# " + text, ""])
            stats["headings"] += 1
        elif style == "Subtitle":
            out.extend(["*" + text + "*", ""])
        elif style in HEADING_LEVELS:
            level = min(HEADING_LEVELS[style], TITLE_LEVEL + 6)
            out.extend(["#" * level + " " + text, ""])
            stats["headings"] += 1
        elif style in ("List Bullet", "List Bullet 2"):
            out.extend(["- " + text, ""])
        elif style in ("List Number", "List Number 2"):
            list_number += 1
            out.extend(["{}. {}".format(list_number, text), ""])
        else:
            if text:
                out.extend([text, ""])
            list_number = 0

        index += 1

    return "\n".join(out).rstrip() + "\n", stats


def tighten_lists(markdown: str) -> str:
    """Drop blank lines between adjacent Markdown list items (tight lists)."""
    pattern = re.compile(r"(?m)^((?:- |\d+\. ).*)\n\n(?=(?:- |\d+\. ))", re.UNICODE)
    while True:
        updated = pattern.sub(r"\1\n", markdown)
        if updated == markdown:
            return updated
        markdown = updated


def verify(markdown: str, docx_path: Path) -> list[str]:
    """Check that every paragraph and table cell survived the conversion."""
    problems: list[str] = []
    doc = Document(str(docx_path))
    blocks = list(iter_block_items(doc))

    for number, block in enumerate(blocks, start=1):
        if isinstance(block, Paragraph):
            for line in block.text.splitlines():
                line = line.strip()
                if line and line not in markdown:
                    problems.append("paragraph text missing: {!r}".format(line[:80]))
        else:
            for row in block.rows:
                for cell in row.cells:
                    for line in cell.text.splitlines():
                        line = line.strip()
                        if line and line not in markdown:
                            problems.append("table cell text missing: {!r}".format(line[:80]))

    table_count = markdown.count("<!-- table ")
    if table_count != len(doc.tables):
        problems.append(
            "table count mismatch: docx has {}, markdown has {}".format(len(doc.tables), table_count)
        )

    if not any(line.startswith("# ") for line in markdown.splitlines()[:5]):
        problems.append("markdown does not start with a level-1 title")
    if not markdown.endswith("\n"):
        problems.append("markdown does not end with a newline")

    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docx", type=Path, default=DEFAULT_DOCX, help="source .docx")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output .md")
    args = parser.parse_args(argv)

    if not args.docx.is_file():
        print("error: source document not found: {}".format(args.docx), file=sys.stderr)
        return 2

    markdown, stats = convert(args.docx)
    markdown = tighten_lists(markdown)
    problems = verify(markdown, args.docx)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(markdown, encoding="utf-8", newline="\n")

    print("source : {}".format(args.docx))
    print("output : {}".format(args.out))
    print(
        "stats  : {} paragraphs, {} tables, {} code blocks, {} headings".format(
            stats["paragraphs"], stats["tables"], stats["code_blocks"], stats["headings"]
        )
    )

    if problems:
        print("verification FAILED ({} problem(s)):".format(len(problems)), file=sys.stderr)
        for problem in problems:
            print("  - " + problem, file=sys.stderr)
        return 1

    print("verification OK: all paragraph text and all {} tables are present".format(stats["tables"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())