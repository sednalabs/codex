"""Verify the packaged TUI renders the version bound by its package manifest."""

import json

from fixtures import SmokePackage
from tui_pty import PackagedTui


def test_initial_tui_header_uses_packaged_version(package: SmokePackage) -> None:
    manifest = json.loads((package.cli_root / "codex-package.json").read_text())
    version = manifest["version"]
    assert version != "0.0.0"

    with PackagedTui(package) as tui:
        initial = tui.until("Ask Codex to do anything")

    assert f"(v{version})" in initial
