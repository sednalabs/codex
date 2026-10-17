"""Focused source-guard tests; the bridge fixture is checker-only, not runtime proof."""

import importlib.util
import pathlib
import shutil
import tempfile
import unittest


STATE = pathlib.Path(__file__).resolve().parent
CHECKER_PATH = STATE / "check_migration_namespace.py"
spec = importlib.util.spec_from_file_location("check_migration_namespace", CHECKER_PATH)
checker = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(checker)


class MigrationNamespaceGuardTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        self.migrations = root / "migrations"
        self.migrations.mkdir()
        self.manifest = root / "manifest.txt"
        shutil.copyfile(STATE / "migration_namespace_manifest.txt", self.manifest)
        for source in (STATE / "migrations").glob("*.sql"):
            shutil.copyfile(source, self.migrations / source.name)
        # This synthetic source satisfies only the guard's text checks. It is
        # not a real bridge, source-closure, or runtime acceptance fixture.
        self.bridge = root / "migration_repair.rs"
        lines = []
        for offset, old in enumerate(checker.EXPECTED_OLD):
            suffix = "" if offset == 0 else f" + {offset}"
            lines.extend(
                [
                    f"const FORK_{old}: i64 = FORK_BASE{suffix};",
                    f"let _ = ({old}, FORK_{old});",
                ]
            )
        mapping = "\n".join(lines)
        self.bridge.write_text(
            "const FORK_BASE: i64 = 8_000_000_000_000_000;\n" + mapping
        )
        self.old = checker.ROOT, checker.MIGRATIONS, checker.MANIFEST, checker.BRIDGE
        checker.ROOT, checker.MIGRATIONS, checker.MANIFEST, checker.BRIDGE = (
            root,
            self.migrations,
            self.manifest,
            self.bridge,
        )

    def tearDown(self):
        checker.ROOT, checker.MIGRATIONS, checker.MANIFEST, checker.BRIDGE = self.old
        self.temp.cleanup()

    def assert_rejected(self):
        with self.assertRaises(ValueError):
            checker.main()

    def test_accepts_upstream_59_and_60_with_frozen_bytes(self):
        checker.main()

    def test_rejects_missing_or_tampered_59_and_60(self):
        for name in (
            "0059_thread_attachment_reverse_lookup.sql",
            "0060_guardian_review_feedback.sql",
        ):
            with self.subTest(name=name, failure="missing"):
                path = self.migrations / name
                path.unlink()
                self.assert_rejected()
                shutil.copyfile(STATE / "migrations" / name, path)
            with self.subTest(name=name, failure="tampered"):
                path = self.migrations / name
                path.write_bytes(path.read_bytes() + b"-- changed\n")
                self.assert_rejected()
                shutil.copyfile(STATE / "migrations" / name, path)

    def test_rejects_renamed_duplicate_and_out_of_range_entries(self):
        original = self.manifest.read_text()
        renamed = original.replace(
            "0059_thread_attachment_reverse_lookup.sql",
            "0059_renamed.sql",
        )
        self.manifest.write_text(renamed)
        self.assert_rejected()

        self.manifest.write_text(original + "u 0059_duplicate.sql " + "0" * 40 + "\n")
        self.assert_rejected()

        out_of_range = original.replace(
            "0060_guardian_review_feedback.sql", "0061_guardian_review_feedback.sql"
        )
        self.manifest.write_text(out_of_range)
        self.assert_rejected()


if __name__ == "__main__":
    unittest.main()
