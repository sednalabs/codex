#!/usr/bin/env python3

import argparse
import os
from pathlib import Path
import subprocess


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Regenerate vendored app-server schema fixtures"
    )
    parser.add_argument(
        "--schema-root",
        type=Path,
        help="root directory containing the schema fixtures",
    )
    parser.add_argument(
        "-p",
        "--prettier",
        type=Path,
        help="optional Prettier executable used to format TypeScript files",
    )
    parser.add_argument(
        "--experimental",
        action="store_true",
        help="regenerate the precomputed experimental exports",
    )
    parser.add_argument(
        "--include-experimental",
        action="store_true",
        help="also regenerate the precomputed experimental exports",
    )
    parser.add_argument(
        "--schema-only",
        action="store_true",
        help="skip regeneration of SDK artifacts derived from repository schemas",
    )
    args = parser.parse_args()

    workspace_root = Path(__file__).resolve().parents[2]
    repository_schema_root = workspace_root / "app-server-protocol" / "schema"
    schema_root = args.schema_root or repository_schema_root

    experimental_modes = [args.experimental]
    if args.include_experimental and not args.experimental:
        experimental_modes.append(True)

    for experimental_api in experimental_modes:
        env = os.environ.copy()
        env["CODEX_APP_SERVER_SCHEMA_ROOT"] = str(schema_root)
        env["CODEX_APP_SERVER_SCHEMA_EXPERIMENTAL"] = (
            "1" if experimental_api else "0"
        )
        if args.prettier:
            env["CODEX_APP_SERVER_SCHEMA_PRETTIER"] = str(args.prettier)

        subprocess.run(
            [
                "cargo",
                "test",
                "-p",
                "codex-app-server-protocol",
                "--lib",
                "schema_fixtures_tests::write_schema_fixtures_from_env",
                "--",
                "--exact",
                "--ignored",
            ],
            cwd=workspace_root,
            env=env,
            check=True,
        )

    # Scratch exports and experimental-only bundles do not update checked-in SDK code.
    if (
        not args.schema_only
        and not args.experimental
        and schema_root.resolve() == repository_schema_root.resolve()
    ):
        sdk_root = workspace_root.parent / "sdk" / "python"
        subprocess.run(
            [
                "uv",
                "run",
                "--project",
                str(sdk_root),
                "--frozen",
                "--only-group",
                "test",
                "python",
                str(sdk_root / "scripts" / "update_sdk_artifacts.py"),
                "generate-types",
                "--schema-dir",
                str(schema_root / "json"),
            ],
            cwd=workspace_root,
            check=True,
        )


if __name__ == "__main__":
    main()
