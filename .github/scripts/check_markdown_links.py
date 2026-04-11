#!/usr/bin/env python3
from __future__ import annotations

import re
import sys
from pathlib import Path
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[2]
DOC_GLOBS = ("README.md", "docs/**/*.md")
INLINE_LINK_RE = re.compile(r"(?<!\!)\[[^\]]+\]\(([^)]+)\)")


def iter_markdown_files() -> list[Path]:
    files: set[Path] = set()
    for pattern in DOC_GLOBS:
        files.update(ROOT.glob(pattern))
    return sorted(path for path in files if path.is_file())


def should_ignore(target: str) -> bool:
    if not target:
        return True
    if target.startswith("#"):
        return True
    if target.startswith(("mailto:", "tel:")):
        return True
    parsed = urlparse(target)
    return parsed.scheme in {"http", "https"}


def resolve_target(source: Path, raw_target: str) -> Path:
    link_target = raw_target.split("#", 1)[0]
    return (source.parent / link_target).resolve()


def main() -> int:
    failures: list[str] = []
    markdown_files = iter_markdown_files()

    for doc_path in markdown_files:
        content = doc_path.read_text(encoding="utf-8")
        for line_no, line in enumerate(content.splitlines(), start=1):
            for match in INLINE_LINK_RE.finditer(line):
                raw_target = match.group(1).strip()
                if should_ignore(raw_target):
                    continue

                target_path = resolve_target(doc_path, raw_target)
                if not target_path.exists():
                    relative_doc = doc_path.relative_to(ROOT)
                    failures.append(
                        f"{relative_doc}:{line_no} -> missing link target {raw_target}"
                    )

    print(f"Checked {len(markdown_files)} markdown files under {ROOT}.")

    if failures:
        print("\nBroken relative markdown links detected:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1

    print("All checked relative markdown links resolved.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
