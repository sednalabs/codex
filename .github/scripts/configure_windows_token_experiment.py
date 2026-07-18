#!/usr/bin/env python3

from __future__ import annotations

import argparse
from pathlib import Path


TOKEN_SOURCE = Path("codex-rs/windows-sandbox-rs/src/token.rs")


def replace_once(source: str, old: str, new: str) -> str:
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one source match, found {count}: {old!r}")
    return source.replace(old, new, 1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "mode",
        choices=(
            "write-restricted-without-everyone",
            "write-restricted-with-everyone",
            "full-restriction-without-everyone",
            "full-restriction-with-everyone",
        ),
    )
    args = parser.parse_args()

    source = TOKEN_SOURCE.read_text(encoding="utf-8")
    if args.mode.endswith("with-everyone"):
        old = """    let mut entries =
        build_restricted_sid_entries(psid_capabilities, extra_restricting_sids, psid_logon);
"""
        new = old + "    entries.push(SID_AND_ATTRIBUTES { Sid: psid_everyone, Attributes: 0 });\n"
        source = replace_once(source, old, new)

    if args.mode.startswith("full-restriction"):
        source = replace_once(
            source,
            "    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;",
            "    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN;",
        )

    TOKEN_SOURCE.write_text(source, encoding="utf-8")


if __name__ == "__main__":
    main()
