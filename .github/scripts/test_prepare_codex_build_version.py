#!/usr/bin/env python3
"""Focused fixtures for source-derived Cargo workspace version preparation."""

import tempfile
import unittest
import unittest.mock
from pathlib import Path


import prepare_codex_build_version as PREPARE


SOURCE_COMMIT = "1" * 40
UPSTREAM_REF_COMMIT = "2" * 40
UPSTREAM_BASE = "3" * 40
TAG_COMMIT = "4" * 40
FINAL_SOURCE_COMMIT = "6" * 40
UPSTREAM_REF = "refs/remotes/upstream/main"
UPSTREAM_TAG = "rust-v0.156.0-alpha.8"
TRACK = "0.156.0-alpha.8"


class PrepareCodexBuildVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.repo = Path(self.temporary_directory.name)
        cargo = self.repo / "codex-rs"
        for package in ("core", "helper", "direct"):
            package_dir = cargo / "crates" / package
            package_dir.mkdir(parents=True)
            contents = f'[package]\nname = "codex-{package}"\nversion.workspace = true\n'
            if package == "core":
                contents += (
                    '\n[dependencies]\n'
                    'codex-helper = { workspace = true }\n'
                    'codex-direct = { path = "../direct" }\n'
                )
            (package_dir / "Cargo.toml").write_text(contents, encoding="utf-8")
        (cargo / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/core"]\n'
            '[workspace.package]\nversion = "0.0.0"\n'
            '[workspace.dependencies]\ncodex-helper = { path = "crates/helper" }\n',
            encoding="utf-8",
        )
        (cargo / "Cargo.lock").write_text(
            'version = 4\n\n'
            '[[package]]\nname = "codex-core"\nversion = "0.0.0"\n'
            'dependencies = ["codex-direct", "codex-helper"]\n\n'
            '[[package]]\nname = "codex-helper"\nversion = "0.0.0"\n\n'
            '[[package]]\nname = "fixture-registry"\nversion = "0.0.0"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
            '[[package]]\nname = "fixture-git"\nversion = "0.0.0"\n'
            'source = "git+https://example.invalid/repo#0123456789abcdef"\n\n'
            '[[package]]\nname = "codex-direct"\nversion = "0.0.0"\n'
            '[metadata]\nversion = "fixture-metadata-version"\n',
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def provenance_mocks(self, source_commit: str = SOURCE_COMMIT):
        return (
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "resolve_commit",
                side_effect=[source_commit, source_commit, UPSTREAM_REF_COMMIT, TAG_COMMIT],
            ),
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "git",
                side_effect=["", str(self.repo), UPSTREAM_BASE],
            ),
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "select_upstream_tag",
                return_value=(
                    PREPARE.VERSION_RESOLVER.SemVer.parse(TRACK),
                    UPSTREAM_TAG,
                    12,
                    False,
                ),
            ),
        )

    def test_provenance_rejects_wrong_head_and_dirty_checkout(self) -> None:
        with unittest.mock.patch.object(
            PREPARE.VERSION_RESOLVER,
            "resolve_commit",
            side_effect=[SOURCE_COMMIT, "5" * 40],
        ):
            with self.assertRaisesRegex(PREPARE.BuildVersionError, "checkout HEAD"):
                PREPARE.resolve_provenance(self.repo, SOURCE_COMMIT, UPSTREAM_REF, TRACK)

        with (
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "resolve_commit",
                side_effect=[SOURCE_COMMIT, SOURCE_COMMIT],
            ),
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "git",
                return_value=" M codex-rs/Cargo.toml",
            ),
        ):
            with self.assertRaisesRegex(PREPARE.BuildVersionError, "must be clean"):
                PREPARE.resolve_provenance(self.repo, SOURCE_COMMIT, UPSTREAM_REF, TRACK)

        with (
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "resolve_commit",
                side_effect=[SOURCE_COMMIT, SOURCE_COMMIT],
            ),
            unittest.mock.patch.object(
                PREPARE.VERSION_RESOLVER,
                "git",
                side_effect=["", str(self.repo.parent)],
            ),
        ):
            with self.assertRaisesRegex(PREPARE.BuildVersionError, "not checkout root"):
                PREPARE.resolve_provenance(self.repo, SOURCE_COMMIT, UPSTREAM_REF, TRACK)

    def test_provenance_reports_exact_base_tag_and_track(self) -> None:
        resolve_commit, git, select_tag = self.provenance_mocks()
        with resolve_commit, git, select_tag:
            result = PREPARE.resolve_provenance(
                self.repo, SOURCE_COMMIT, UPSTREAM_REF, TRACK
            )
        self.assertEqual(
            result,
            PREPARE.VersionProvenance(
                source_commit=SOURCE_COMMIT,
                upstream_ref=UPSTREAM_REF,
                upstream_ref_commit=UPSTREAM_REF_COMMIT,
                upstream_base=UPSTREAM_BASE,
                upstream_tag=UPSTREAM_TAG,
                upstream_tag_commit=TAG_COMMIT,
                upstream_track=TRACK,
                tag_distance=12,
                exact_tag_base=False,
            ),
        )

    def test_provenance_rejects_mismatched_track_and_non_exact_source(self) -> None:
        with self.assertRaisesRegex(PREPARE.BuildVersionError, "not expected"):
            mocks = self.provenance_mocks()
            with mocks[0], mocks[1], mocks[2]:
                PREPARE.resolve_provenance(
                    self.repo, SOURCE_COMMIT, UPSTREAM_REF, "0.155.0-alpha.8"
                )
        with self.assertRaisesRegex(PREPARE.BuildVersionError, "full 40-character"):
            PREPARE.resolve_provenance(self.repo, "1" * 8, UPSTREAM_REF, TRACK)

    def test_write_changes_only_workspace_and_source_free_local_lock_versions(self) -> None:
        mocks = self.provenance_mocks()
        with mocks[0], mocks[1], mocks[2]:
            result = PREPARE.run(
                self.repo, SOURCE_COMMIT, UPSTREAM_REF, TRACK, "write"
            )

        cargo = self.repo / "codex-rs"
        manifest = (cargo / "Cargo.toml").read_text(encoding="utf-8")
        lock = (cargo / "Cargo.lock").read_text(encoding="utf-8")
        self.assertIn(f'version = "{TRACK}"', manifest)
        self.assertEqual(result["local_lock_entries_updated"], 3)
        self.assertEqual(lock.count(f'version = "{TRACK}"'), 3)
        self.assertIn(
            'name = "fixture-registry"\nversion = "0.0.0"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"',
            lock,
        )
        self.assertIn(
            'name = "fixture-git"\nversion = "0.0.0"\n'
            'source = "git+https://example.invalid/repo#0123456789abcdef"',
            lock,
        )
        self.assertIn('[metadata]\nversion = "fixture-metadata-version"', lock)

        mocks = self.provenance_mocks(FINAL_SOURCE_COMMIT)
        with mocks[0], mocks[1], mocks[2]:
            checked = PREPARE.run(
                self.repo, FINAL_SOURCE_COMMIT, UPSTREAM_REF, TRACK, "check"
            )
        self.assertEqual(checked["workspace_version_before"], TRACK)
        self.assertEqual(checked["local_lock_entries_updated"], 0)

    def test_write_does_not_rewrite_an_already_prepared_workspace(self) -> None:
        cargo = self.repo / "codex-rs"
        manifest = cargo / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace('"0.0.0"', f'"{TRACK}"'),
            encoding="utf-8",
        )
        lock = cargo / "Cargo.lock"
        lock_text = lock.read_text(encoding="utf-8")
        for name in ("codex-core", "codex-helper", "codex-direct"):
            start = lock_text.index(f'name = "{name}"')
            version_start = lock_text.index('version = "0.0.0"', start)
            lock_text = (
                lock_text[:version_start]
                + f'version = "{TRACK}"'
                + lock_text[version_start + len('version = "0.0.0"') :]
            )
        lock.write_text(lock_text, encoding="utf-8")

        mocks = self.provenance_mocks(FINAL_SOURCE_COMMIT)
        with (
            mocks[0],
            mocks[1],
            mocks[2],
            unittest.mock.patch.object(Path, "write_text") as write_text,
        ):
            PREPARE.run(self.repo, FINAL_SOURCE_COMMIT, UPSTREAM_REF, TRACK, "write")
        write_text.assert_not_called()

    def test_write_rejects_unexpected_prior_workspace_version(self) -> None:
        cargo = self.repo / "codex-rs"
        manifest = cargo / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace('"0.0.0"', '"0.145.0"'),
            encoding="utf-8",
        )
        lock = cargo / "Cargo.lock"
        lock_text = lock.read_text(encoding="utf-8")
        for name in ("codex-core", "codex-helper", "codex-direct"):
            start = lock_text.index(f'name = "{name}"')
            version_start = lock_text.index('version = "0.0.0"', start)
            lock_text = (
                lock_text[:version_start]
                + 'version = "0.145.0"'
                + lock_text[version_start + len('version = "0.0.0"') :]
            )
        lock.write_text(lock_text, encoding="utf-8")
        state = PREPARE.load_workspace_state(self.repo)
        with self.assertRaisesRegex(PREPARE.BuildVersionError, "unexpected prior"):
            PREPARE.prepare_workspace(state, TRACK, "write")

    def test_check_rejects_placeholder_and_duplicate_local_lock_entries(self) -> None:
        state = PREPARE.load_workspace_state(self.repo)
        with self.assertRaisesRegex(PREPARE.BuildVersionError, "expected source track"):
            PREPARE.prepare_workspace(state, TRACK, "check")

        lock = self.repo / "codex-rs" / "Cargo.lock"
        lock.write_text(
            lock.read_text(encoding="utf-8")
            + '\n[[package]]\nname = "codex-core"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        with self.assertRaisesRegex(PREPARE.BuildVersionError, "found 2"):
            PREPARE.load_workspace_state(self.repo)


if __name__ == "__main__":
    unittest.main()
